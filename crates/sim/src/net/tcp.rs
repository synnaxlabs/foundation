//! TCP streams and listeners. A segment is never duplicated, a lost one goes again,
//! and the segments of one direction of a stream arrive in the order they were sent.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::IoSlice;
use std::net::SocketAddr;
use std::task::{self, Poll};
use std::{iter, mem};

use env::net::{Error, tcp};
use types::time::{Monotonic, Span};

use super::wire::{Packet, Wire};
use super::{EPHEMERAL, Fate, NOT_AVAILABLE, addresses, ip_header, node, receives};
use crate::EIO;
use crate::chance::roll;
use crate::log::Waker;

/// The key and the pair of the end of a stream that a listener accepts, or its error.
type Accepted = Result<(u64, Pair), Error>;

/// The bytes of the TCP header of a segment, with no options.
const HEADER: usize = 20;
/// The least segment size, as Linux clamps it (`tcp_min_snd_mss`).
const MSS_MIN: usize = 48;
/// The Linux code for a write after a close (`EPIPE`).
const BROKEN_PIPE: i32 = 32;

/// The RTO of a SYN or a SYN-ACK, as on Linux (`TCP_TIMEOUT_INIT`).
const RTO_SYN: Span = Span::SECOND;
/// The RTO of another segment past its round trip, as on Linux (`tcp_rto_min`).
const RTO_MIN: Span = Span::from_nanos(200 * Span::MILLISECOND.nanos());
/// The most RTO after each loss doubles it, as on Linux (`TCP_RTO_MAX`).
const RTO_MAX: Span = Span::from_nanos(120 * Span::SECOND.nanos());
/// The time from the first send of a lost segment of a stream to the timeout of its
/// end: 15 tries from [`RTO_MIN`], as Linux models `tcp_retries2`.
const WRITE_TIMEOUT: Span = Span::from_nanos(924_600 * Span::MILLISECOND.nanos());
/// The time from the first SYN of a connect to its timeout: 7 tries from
/// [`RTO_SYN`], as on Linux (`tcp_syn_retries`).
const CONNECT_TIMEOUT: Span = Span::from_nanos(127 * Span::SECOND.nanos());

const BACKLOG: &str = "sim does not simulate a full TCP backlog yet";
const REOPENED: &str = "sim does not simulate a SYN to a live TCP stream yet";

/// The address of an end of a stream and its peer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pair {
    pub(crate) local: SocketAddr,
    pub(crate) peer: SocketAddr,
}

pub(super) struct Segment {
    source: SocketAddr,
    destination: SocketAddr,
    /// The key of its stream.
    stream: u64,
    kind: Kind,
    tries: Tries,
}

/// A segment. An edge is the count of bytes from the start of the stream that its
/// sender can take.
enum Kind {
    Syn {
        edge: usize,
    },
    SynAck {
        edge: usize,
    },
    /// `received` bytes arrived, and the FIN when `fin`.
    Ack {
        received: usize,
        edge: usize,
        fin: bool,
    },
    Data(Vec<u8>),
    Fin,
    /// It answers a segment at a closed port or a refusal, or stands for the lost
    /// segments of an end that left, and goes again as the next segment of the end
    /// that gets it, of class `0`, would. An abort sends `None`.
    Rst(Option<Retry>),
}

impl Kind {
    /// The bytes it carries.
    fn len(&self) -> usize {
        match self {
            Self::Data(bytes) => bytes.len(),
            _ => 0,
        }
    }

    /// Whether it is an ACK or an RST. It waits behind no lost segment, and the next
    /// answer of its stream in its direction takes its place when it is lost.
    fn answer(&self) -> bool {
        matches!(self, Self::Ack { .. } | Self::Rst(_))
    }

    /// How it goes again when it is lost, or `None` when it never does.
    fn retry(&self) -> Option<Retry> {
        match self {
            Self::Syn { .. } => Some(Retry::Connect),
            Self::SynAck { .. } => Some(Retry::Accept),
            Self::Ack { .. } | Self::Data(_) | Self::Fin => Some(Retry::Stream),
            Self::Rst(retry) => *retry,
        }
    }
}

/// How a lost segment goes again: its RTO, and when the end that waits on it times
/// out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// A SYN, or an RST to an end that connects. The connect ends at its own
    /// deadline.
    Connect,
    /// A SYN-ACK, or an RST to an end that accepts.
    Accept,
    /// Bytes, a FIN, an ACK, or an RST to another end.
    Stream,
}

/// Why an end sends nothing more and takes no segment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Broken {
    /// An RST arrived.
    Reset,
    /// A segment that it waits on was lost each time it went.
    TimedOut,
}

/// Where an end is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Its first SYN went at true time `0`, and its connect waits, or failed once it
    /// broke.
    Connecting(Monotonic),
    /// It answered a SYN to listener `0`, and waits for the ACK.
    Accepting(u64),
    /// It waits for an accept of listener `0`.
    Queued(u64),
    /// A driver holds it.
    Open,
    /// Its driver dropped after its close. It sends its bytes and its FIN, then goes
    /// once the peer has them and the peer's FIN has arrived.
    Orphan,
}

/// How far the FIN of an end has gone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fin {
    Unwritten,
    /// It goes after the bytes not yet sent.
    Queued,
    Sent,
    Acked,
}

struct End {
    pair: Pair,
    /// The key of its stream.
    stream: u64,
    phase: Phase,
    options: tcp::Options,
    /// The bytes that arrived and were not read.
    inbox: VecDeque<u8>,
    read: usize,
    /// The edge in the last segment to the peer.
    advertised: usize,
    /// The peer's FIN arrived.
    peer_closed: bool,
    broken: Option<Broken>,
    /// The bytes written and not yet sent.
    outbox: VecDeque<u8>,
    sent: usize,
    /// The bytes that the peer has.
    acked: usize,
    /// The peer's edge.
    edge: usize,
    fin: Fin,
    reading: Option<Waker>,
    /// The waker of a write or of a connect.
    writing: Option<Waker>,
}

impl End {
    fn new(
        pair: Pair,
        stream: u64,
        phase: Phase,
        options: tcp::Options,
        edge: usize,
    ) -> Self {
        Self {
            pair,
            stream,
            phase,
            options,
            inbox: VecDeque::new(),
            read: 0,
            advertised: options.recv_buffer_bytes,
            peer_closed: false,
            broken: None,
            outbox: VecDeque::new(),
            sent: 0,
            acked: 0,
            edge,
            fin: Fin::Unwritten,
            reading: None,
            writing: None,
        }
    }

    /// The ACK of what has arrived so far.
    fn ack(&self) -> Kind {
        Kind::Ack {
            received: self.read + self.inbox.len(),
            edge: self.advertised,
            fin: self.peer_closed,
        }
    }

    /// Whether the stream has ended: it broke or it closed. As on Linux, the end then
    /// leaves its pair and frees its port, and a drop of it sends nothing.
    fn done(&self) -> bool {
        self.broken.is_some() || self.closed()
    }

    /// Whether each FIN arrived and its own is acked.
    fn closed(&self) -> bool {
        self.fin == Fin::Acked && self.peer_closed
    }

    /// The error of a call on it once it broke.
    fn error(&self, broken: Broken) -> Error {
        let remote = self.pair.peer;
        match broken {
            Broken::Reset => Error::Reset { remote },
            Broken::TimedOut => Error::TimedOut { remote },
        }
    }

    /// How a lost RST to it goes again: as the segment that it would send next.
    fn retry(&self) -> Retry {
        match self.phase {
            Phase::Connecting(_) => Retry::Connect,
            Phase::Accepting(_) => Retry::Accept,
            Phase::Queued(_) | Phase::Open | Phase::Orphan => Retry::Stream,
        }
    }

    /// Whether it waits for an answer from its peer: to its SYN or its SYN-ACK, or
    /// for its bytes, its FIN, or room for its bytes.
    fn waits(&self) -> bool {
        matches!(self.phase, Phase::Connecting(_) | Phase::Accepting(_))
            || self.acked < self.sent
            || !self.outbox.is_empty()
            || self.fin == Fin::Sent
    }
}

/// A listener and the streams that it has not accepted.
struct Listening {
    node: usize,
    local: SocketAddr,
    /// The most streams that wait: the backlog plus 1, as on Linux.
    queue_max: usize,
    options: tcp::Options,
    /// The keys of its ends in [`Phase::Accepting`] and [`Phase::Queued`], in the
    /// order of their SYNs.
    queue: VecDeque<u64>,
    waker: Option<Waker>,
    /// From a fault until the listener drops: its queue holds only queued streams,
    /// each accept gives `EIO` once the queue is empty, and a SYN gets an RST.
    failed: bool,
}

/// The order of the segments in flight in each direction, the lost ones and their
/// next tries, and the first case that sim does not simulate yet.
#[derive(Default)]
struct Lanes {
    /// The latest arrival in flight in each direction, by source and destination,
    /// and the count of segments in flight there.
    floors: BTreeMap<(SocketAddr, SocketAddr), (Monotonic, usize)>,
    /// The lost segment of each stream in each direction that has one, and the
    /// segments other than answers sent there after it, which wait for it as a
    /// receiver holds the bytes after a gap. By source, destination, and stream.
    held: BTreeMap<(SocketAddr, SocketAddr, u64), Held>,
    /// The lost answer of each stream in each direction, by source, destination, and
    /// stream.
    answers: BTreeMap<(SocketAddr, SocketAddr, u64), Answer>,
    yet: Option<&'static str>,
}

/// The tries of a segment so far.
#[derive(Clone, Copy)]
struct Tries {
    /// The true time from which the end that waits on it counts its timeout: its
    /// first send, or its hold for a held segment, as a later segment goes at its
    /// write on Linux.
    since: Monotonic,
    /// How many times it was lost.
    losses: u32,
}

impl Segment {
    /// How it goes again, as it was lost.
    fn retry(&self) -> Retry {
        (self.kind.retry()).expect("invariant: a lost segment goes again")
    }

    /// Counts a loss of it at true time `now`, and gives the true time of its next
    /// try, or `None` past `u64` nanoseconds, where it never goes again.
    fn lose(&mut self, wire: &Wire, now: Monotonic) -> Option<Monotonic> {
        self.tries.losses += 1;
        now.checked_add(rto(wire, self, self.tries.losses))
    }

    /// Whether the end that waits on it times out at true time `now`, in place of
    /// its next try. As on Linux, a SYN-ACK goes 6 times (`tcp_synack_retries`), and
    /// a segment of a stream until [`WRITE_TIMEOUT`]. A connect ends at its own
    /// deadline.
    fn timed_out(&self, now: Monotonic) -> bool {
        match self.retry() {
            Retry::Connect => false,
            Retry::Accept => self.tries.losses > 5,
            Retry::Stream => now - self.tries.since >= WRITE_TIMEOUT,
        }
    }
}

/// The segments that wait in one direction of a stream.
struct Held {
    /// The true time of the next try of the first.
    at: Monotonic,
    segments: VecDeque<Segment>,
}

/// A lost ACK or RST. As ACKs are cumulative and an end that is gone answers each
/// segment with an RST, the next answer of its stream in its direction carries it,
/// and it goes again only when none arrives first.
struct Answer {
    /// The true time of its next try, or `None` while no end waits on it. It is
    /// armed again when its end waits.
    at: Option<Monotonic>,
    /// The segments in flight in its direction at its last loss, which arrive
    /// before one that can carry it.
    ahead: usize,
    segment: Segment,
}

impl Lanes {
    /// Sends a segment of `kind` of `stream` from end `from` at true time `now`. A
    /// segment other than an answer goes after the segments that its stream holds in
    /// its direction.
    fn send(
        &mut self,
        wire: &mut Wire,
        now: Monotonic,
        from: Pair,
        stream: u64,
        kind: Kind,
    ) {
        let (source, destination) = (from.local, from.peer);
        let answer = kind.answer();
        let segment = Segment {
            source,
            destination,
            stream,
            kind,
            tries: Tries {
                since: now,
                losses: 0,
            },
        };
        match self.held.get_mut(&(source, destination, stream)) {
            Some(held) if !answer => held.segments.push_back(segment),
            _ => self.release(wire, now, [segment]),
        }
    }

    /// Sends at true time `now` the RST of class `retry` of `stream` from end `from`,
    /// which answers a segment of `tries` at a closed port or a refusal. As the
    /// sender of that segment sends it again and meets the same answer, the RST
    /// continues its tries.
    fn refuse(
        &mut self,
        wire: &mut Wire,
        now: Monotonic,
        from: Pair,
        stream: u64,
        retry: Retry,
        tries: Tries,
    ) {
        let segment = Segment {
            source: from.local,
            destination: from.peer,
            stream,
            kind: Kind::Rst(Some(retry)),
            tries,
        };
        self.release(wire, now, [segment]);
    }

    /// Sends `segments` of one direction of a stream in order at true time `now`,
    /// until one that is lost holds the rest. A lost answer holds nothing. A segment
    /// lost past `u64` nanoseconds never goes again, and the rest wait on it.
    fn release(
        &mut self,
        wire: &mut Wire,
        now: Monotonic,
        segments: impl IntoIterator<Item = Segment>,
    ) {
        let mut segments = segments.into_iter();
        while let Some(segment) = segments.next() {
            let Err(mut lost) = self.transmit(wire, now, segment) else {
                continue;
            };
            let Some(at) = lost.lose(wire, now) else {
                return;
            };
            if lost.kind.answer() {
                self.keep(Some(at), lost);
            } else {
                let key = (lost.source, lost.destination, lost.stream);
                let segments = iter::once(lost).chain(segments).collect();
                self.held.insert(key, Held { at, segments });
                return;
            }
        }
    }

    /// Puts `segment` on its link at true time `now`. It leaves its link as
    /// [`Wire::depart`] gives, then it is lost as the link's loss draws, or arrives
    /// after the delay and a jitter draw of the link, and not before a segment in
    /// flight in its direction. A link with no loss makes no draw of it. An answer
    /// that goes takes the place of the lost answer of its stream and direction.
    /// Gives back a lost segment that goes again.
    fn transmit(
        &mut self,
        wire: &mut Wire,
        now: Monotonic,
        segment: Segment,
    ) -> Result<(), Segment> {
        let (source, destination) = (segment.source, segment.destination);
        let node = node(source.ip()).expect("invariant: an end is on a node");
        let path = wire.path(node, destination.ip());
        let lost = path.link.loss > 0.0 && roll(wire.rng(), path.link.loss);
        let fate = if lost { Fate::Lost } else { Fate::Sent };
        let (tag, len) = (mem::discriminant(&segment.kind), segment.kind.len());
        wire.record((now, source, destination, tag, len, fate));
        let bytes = len + header(destination);
        let Some(departure) = wire.depart(now, &path, bytes) else {
            return Ok(());
        };
        if lost {
            return match segment.kind.retry() {
                Some(_) => Err(segment),
                None => Ok(()),
            };
        }
        let Some(at) = wire.draw(&path, departure) else {
            return Ok(());
        };
        if segment.kind.answer() {
            self.answers.remove(&(source, destination, segment.stream));
        }
        let at = self.raise(source, destination, at);
        wire.put(&path, departure, at, Packet::Segment(segment));
        Ok(())
    }

    /// Keeps `lost`, an answer with its next try at `at`, in place of a lost answer
    /// of its stream and direction. When that one has a next try, `lost` takes its
    /// try and its tries.
    fn keep(&mut self, at: Option<Monotonic>, mut lost: Segment) {
        let (source, destination) = (lost.source, lost.destination);
        let ahead = self
            .floors
            .get(&(source, destination))
            .map_or(0, |floor| floor.1);
        match self.answers.entry((source, destination, lost.stream)) {
            Entry::Occupied(mut answer) => {
                let answer = answer.get_mut();
                if answer.at.is_some() {
                    lost.tries = answer.segment.tries;
                } else {
                    answer.at = at;
                }
                (answer.ahead, answer.segment) = (ahead, lost);
            }
            Entry::Vacant(answer) => {
                answer.insert(Answer {
                    at,
                    ahead,
                    segment: lost,
                });
            }
        }
    }

    /// Arms the lost answer to end `to` of `stream` that no end waited on, as the
    /// end now waits on it: it goes again after its RTO from true time `now`, and
    /// times out from `now`.
    fn arm(&mut self, wire: &Wire, now: Monotonic, to: Pair, stream: u64) {
        let key = (to.peer, to.local, stream);
        if let Some(answer) = self.answers.get_mut(&key)
            && answer.at.is_none()
        {
            let segment = &mut answer.segment;
            segment.tries = Tries {
                since: now,
                losses: 0,
            };
            answer.at = segment.lose(wire, now);
        }
    }

    /// The true time at which the first lost segment goes again or times out.
    fn first(&self) -> Option<Monotonic> {
        let held = self.held.values().map(|held| held.at);
        held.chain(self.answers.values().filter_map(|answer| answer.at))
            .min()
    }

    /// Sends again each lost segment due by true time `now`, then the segments that
    /// waited on it. `waits` gives whether the end that waits on a lost answer does,
    /// or `None` when no end takes it: an answer that no end waits on waits with no
    /// timer, and one that no end takes drops. Gives each lost segment whose end
    /// that waits on it times out, and drops the segments that waited on it.
    fn retry(
        &mut self,
        wire: &mut Wire,
        now: Monotonic,
        waits: impl Fn(&Segment) -> Option<bool>,
    ) -> Vec<Segment> {
        let mut failed = Vec::new();
        let due: Vec<_> = (self.held.iter())
            .filter(|(_, held)| held.at <= now)
            .map(|(&key, _)| key)
            .collect();
        for key in due {
            let Held { mut segments, .. } =
                self.held.remove(&key).expect("invariant: found");
            let first = segments.pop_front().expect("invariant: held");
            if first.timed_out(now) {
                failed.push(first);
                continue;
            }
            match self.transmit(wire, now, first) {
                Ok(()) => self.release(wire, now, segments),
                Err(mut lost) => {
                    if let Some(at) = lost.lose(wire, now) {
                        segments.push_front(lost);
                        self.held.insert(key, Held { at, segments });
                    }
                }
            }
        }
        let due: Vec<_> = (self.answers.iter())
            .filter(|(_, answer)| answer.at.is_some_and(|at| at <= now))
            .map(|(&key, _)| key)
            .collect();
        for key in due {
            let Entry::Occupied(mut answer) = self.answers.entry(key) else {
                unreachable!("invariant: found");
            };
            match waits(&answer.get().segment) {
                Some(true) => {}
                Some(false) => {
                    answer.get_mut().at = None;
                    continue;
                }
                None => {
                    answer.remove();
                    continue;
                }
            }
            let Answer { segment, .. } = answer.remove();
            if segment.timed_out(now) {
                failed.push(segment);
            } else if let Err(mut lost) = self.transmit(wire, now, segment)
                && let Some(at) = lost.lose(wire, now)
            {
                self.keep(Some(at), lost);
            }
        }
        failed
    }

    /// Takes the lost segments of `stream` from end `from`, which has gone. When
    /// `to` gives the class of a lost RST to the end of the stream at its peer, as
    /// its peer sends again what waits on them and meets a closed port, a lost RST
    /// of that class stays: the lost RST, or else the held SYN-ACK or the lost ACK
    /// as an RST with its tries and its next try. The rest drop. Gives whether a
    /// lost RST stays.
    fn leave(&mut self, from: Pair, stream: u64, to: Option<Retry>) -> bool {
        let key = (from.local, from.peer, stream);
        let held = self.held.remove(&key);
        let answer = self.answers.remove(&key);
        let Some(retry) = to else {
            return false;
        };
        let syn_ack = held.and_then(|Held { at, mut segments }| {
            let first = segments.pop_front().expect("invariant: held");
            matches!(first.kind, Kind::SynAck { .. }).then_some((Some(at), first))
        });
        let answer = answer.map(|Answer { at, ahead, segment }| {
            let rst = matches!(segment.kind, Kind::Rst(_));
            (at, ahead, segment, rst)
        });
        if let Some((at, ahead, mut segment, true)) = answer {
            segment.kind = Kind::Rst(Some(retry));
            self.answers.insert(key, Answer { at, ahead, segment });
            return true;
        }
        let ack = answer.map(|(at, _, segment, _)| (at, segment));
        let Some((at, mut lost)) = syn_ack.or(ack) else {
            return false;
        };
        lost.kind = Kind::Rst(Some(retry));
        self.keep(at, lost);
        true
    }

    /// Adds a segment in flight from `source` to `destination` that arrives at true
    /// time `at`, and gives when it arrives: at `at`, or with the last segment in
    /// flight in its direction if that one arrives later.
    fn raise(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        at: Monotonic,
    ) -> Monotonic {
        let floor = self.floors.entry((source, destination)).or_insert((at, 0));
        floor.0 = floor.0.max(at);
        floor.1 += 1;
        floor.0
    }

    /// Counts again the segments that `node` has in flight, after its power cut
    /// dropped those that had not left it.
    fn cut(&mut self, wire: &Wire, node: usize) {
        let own = addresses(node);
        self.floors
            .retain(|(source, _), _| !own.contains(&source.ip()));
        for (at, packet) in wire.flights() {
            if let Packet::Segment(segment) = packet
                && own.contains(&segment.source.ip())
            {
                self.raise(segment.source, segment.destination, at);
            }
        }
    }

    /// Drops the lost segments from `node`, whose ends are gone after its power cut,
    /// other than its RSTs, as its peers meet its closed ports.
    fn fall(&mut self, node: usize) {
        let own = addresses(node);
        let gone = |source: &SocketAddr| own.contains(&source.ip());
        self.held.retain(|(source, ..), _| !gone(source));
        self.answers.retain(|(source, ..), answer| {
            !gone(source) || matches!(answer.segment.kind, Kind::Rst(_))
        });
    }

    /// Notes the arrival of a segment of `stream` from `source` to `destination`,
    /// and gives the lost answer that it carries.
    fn arrive(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        stream: u64,
    ) -> Option<Kind> {
        let Entry::Occupied(mut floor) = self.floors.entry((source, destination))
        else {
            panic!("invariant: a segment in flight has its floor");
        };
        floor.get_mut().1 -= 1;
        if floor.get().1 == 0 {
            floor.remove();
        }
        let mut carried = None;
        let lane = (source, destination, 0)..=(source, destination, u64::MAX);
        for (&key, answer) in self.answers.range_mut(lane) {
            if answer.ahead > 0 {
                answer.ahead -= 1;
            } else if key.2 == stream {
                carried = Some(key);
            }
        }
        (self.answers.remove(&carried?)).map(|answer| answer.segment.kind)
    }
}

/// The TCP ends and listeners of a run.
#[derive(Default)]
pub(super) struct Sockets {
    ends: BTreeMap<u64, End>,
    /// The key of the end on each pair. No end in it is done.
    routes: BTreeMap<Pair, u64>,
    listeners: BTreeMap<u64, Listening>,
    /// The port of the last connect of each node.
    ports: BTreeMap<usize, u16>,
    /// The last key given to an end, a listener, or a stream.
    last: u64,
    /// The true time of the first SYN and the key of each end that connects.
    connects: BTreeSet<(Monotonic, u64)>,
    lanes: Lanes,
}

impl Sockets {
    /// Takes the first case met that sim does not simulate yet.
    pub(super) fn yet(&mut self) -> Option<&'static str> {
        self.lanes.yet.take()
    }

    /// The true time at which the first lost segment goes again or times out, or a
    /// connect times out.
    pub(super) fn first(&self) -> Option<Monotonic> {
        let connect = (self.connects.first())
            .and_then(|&(since, _)| since.checked_add(CONNECT_TIMEOUT));
        self.lanes.first().into_iter().chain(connect).min()
    }

    /// The key of the listener that takes a SYN to `local`: one that receives it and
    /// has not failed.
    fn listener(&self, local: SocketAddr) -> Option<u64> {
        (self.listeners.iter())
            .find(|(_, listening)| {
                !listening.failed && receives(listening.node, listening.local, local)
            })
            .map(|(&key, _)| key)
    }

    /// A key that no end, listener, or stream has had.
    fn key(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    /// Adds `end` and gives its key.
    fn insert(&mut self, end: End) -> u64 {
        let key = self.key();
        let routed = self.routes.insert(end.pair, key);
        assert!(
            routed.is_none(),
            "invariant: a pair has one end that is not done"
        );
        self.ends.insert(key, end);
        key
    }

    /// Removes end `key` at true time `now`.
    fn remove(&mut self, wire: &Wire, now: Monotonic, key: u64) -> End {
        let routed = self.unroute(wire, now, key);
        let end = (self.ends.remove(&key)).expect("invariant: the end lives");
        assert!(
            routed || end.done(),
            "invariant: an end that is not done has its pair"
        );
        end
    }

    /// Takes the segments to its pair from end `key` at true time `now`, and gives
    /// whether it had them. When it had them, its connect ends, the lost answers to
    /// it drop, and unless its stream closed, it leaves as [`Self::leave`] says.
    fn unroute(&mut self, wire: &Wire, now: Monotonic, key: u64) -> bool {
        let end = &self.ends[&key];
        let (pair, stream) = (end.pair, end.stream);
        let routed = self.routes.get(&pair) == Some(&key);
        if !routed {
            return false;
        }
        self.routes.remove(&pair);
        if let Phase::Connecting(since) = end.phase {
            self.connects.remove(&(since, key));
        }
        self.lanes.answers.remove(&(pair.peer, pair.local, stream));
        if !end.closed() {
            self.leave(wire, now, pair, stream);
        }
        true
    }

    /// Takes the lost segments of `stream` from end `from`, which leaves its pair
    /// before its stream closed at true time `now`. As its peer sends again, or sends
    /// its next bytes, and meets a closed port, the peer gets one lost RST: in place
    /// of a lost one as [`Lanes::leave`] says, or else one that goes while the peer
    /// waits and has no timer while it does not. A connecting peer gets none while a
    /// listener takes its next SYN.
    fn leave(&mut self, wire: &Wire, now: Monotonic, from: Pair, stream: u64) {
        let to = Pair {
            local: from.peer,
            peer: from.local,
        };
        let peer = (self.routes.get(&to))
            .map(|key| &self.ends[key])
            .filter(|peer| peer.stream == stream);
        let listens = self.listener(from.local).is_some();
        let retry =
            (peer.map(End::retry)).filter(|&retry| retry != Retry::Connect || !listens);
        if self.lanes.leave(from, stream, retry) {
            return;
        }
        let (Some(peer), Some(retry)) = (peer, retry) else {
            return;
        };
        let mut rst = Segment {
            source: from.local,
            destination: from.peer,
            stream,
            kind: Kind::Rst(Some(retry)),
            tries: Tries {
                since: now,
                losses: 0,
            },
        };
        let at = if peer.waits() {
            rst.lose(wire, now)
        } else {
            None
        };
        self.lanes.keep(at, rst);
    }
}

/// The key of the end in `ends` that waits on `segment` while it is lost: the
/// receiver of an answer, and the sender of another segment.
fn owner(
    ends: &BTreeMap<u64, End>,
    routes: &BTreeMap<Pair, u64>,
    segment: &Segment,
) -> Option<u64> {
    let (local, peer) = if segment.kind.answer() {
        (segment.destination, segment.source)
    } else {
        (segment.source, segment.destination)
    };
    let key = *routes.get(&Pair { local, peer })?;
    (ends[&key].stream == segment.stream).then_some(key)
}

/// The TCP ends and listeners of a run, with the wire they send on.
pub(crate) struct Tcp<'a> {
    sockets: &'a mut Sockets,
    wire: &'a mut Wire,
}

/// The largest data segment from `from` on its link.
fn mss(wire: &Wire, from: Pair) -> usize {
    let node = node(from.local.ip()).expect("invariant: an end is on a node");
    let mtu = wire.path(node, from.peer.ip()).link.mtu;
    mtu.saturating_sub(header(from.peer)).max(MSS_MIN)
}

/// The time after which `segment`, lost `losses` times, goes again: the RTO of its
/// kind, doubled after each loss but the first, to at most [`RTO_MAX`]. The RTO of a
/// handshake segment is [`RTO_SYN`], and that of a segment of a stream is the round
/// trip of its links at their most delay, plus [`RTO_MIN`].
fn rto(wire: &Wire, segment: &Segment, losses: u32) -> Span {
    let (source, destination) = (segment.source, segment.destination);
    let rto = if segment.retry() == Retry::Stream {
        let from = node(source.ip()).expect("invariant: an end is on a node");
        let to = node(destination.ip()).expect("invariant: a peer is on a node");
        let [out, back] = [(from, destination), (to, source)]
            .map(|(node, peer)| wire.path(node, peer.ip()).link);
        let nanos = [out.delay, out.jitter, back.delay, back.jitter, RTO_MIN]
            .iter()
            .fold(0i64, |sum, span| sum.saturating_add(span.nanos()));
        Span::from_nanos(nanos)
    } else {
        RTO_SYN
    };
    let doubled = rto.nanos().saturating_mul(1 << (losses - 1).min(32));
    Span::from_nanos(doubled.min(RTO_MAX.nanos()))
}

/// The bytes of the IP and TCP headers of a segment to `peer`.
fn header(peer: SocketAddr) -> usize {
    HEADER + ip_header(peer.ip())
}

impl<'a> Tcp<'a> {
    pub(super) fn new(sockets: &'a mut Sockets, wire: &'a mut Wire) -> Self {
        Self { sockets, wire }
    }

    /// Listens on `node`, and gives the key and the local address of the listener.
    pub(crate) fn listen(
        &mut self,
        node: usize,
        config: &tcp::Listen,
    ) -> Result<(u64, SocketAddr), Error> {
        let bound = (self.sockets.listeners.values())
            .filter(|listening| listening.node == node)
            .map(|listening| listening.local);
        let local = super::bind(node, config.local, &bound)?;
        let backlog = usize::try_from(config.backlog).unwrap_or(usize::MAX);
        let listening = Listening {
            node,
            local,
            queue_max: backlog.saturating_add(1),
            options: config.options,
            queue: VecDeque::new(),
            waker: None,
            failed: false,
        };
        let listener = self.sockets.key();
        self.sockets.listeners.insert(listener, listening);
        Ok((listener, local))
    }

    /// Stops `listener`, and resets the streams that it has not accepted. A
    /// listener that a power cut ended is gone already. Returns the waker of its
    /// accept, for the caller to drop after it releases the lock.
    pub(crate) fn unlisten(&mut self, now: Monotonic, listener: u64) -> Option<Waker> {
        let listening = self.sockets.listeners.remove(&listener)?;
        for key in listening.queue {
            self.abort(now, key);
        }
        listening.waker
    }

    /// The count of the ends of `node` that a driver holds.
    pub(crate) fn streams(&self, node: usize) -> usize {
        let own = addresses(node);
        (self.sockets.ends.values())
            .filter(|end| own.contains(&end.pair.local.ip()))
            .filter(|end| matches!(end.phase, Phase::Connecting(_) | Phase::Open))
            .count()
    }

    /// Makes the listener of `node` at `local` fail, and resets the streams in its
    /// handshake. Returns a waker for the caller to wake after it releases the lock:
    /// that of an accept that waits, or one that does nothing. Returns `None` when no
    /// listener of `node` is at `local`.
    pub(crate) fn fail(
        &mut self,
        now: Monotonic,
        node: usize,
        local: SocketAddr,
    ) -> Option<Waker> {
        let (&listener, listening) =
            (self.sockets.listeners.iter_mut()).find(|(_, listening)| {
                listening.node == node && listening.local == local
            })?;
        listening.failed = true;
        let waker = (listening.waker.take()).unwrap_or_else(Waker::noop);
        let ends = &self.sockets.ends;
        let (accepting, queued) = (listening.queue.iter())
            .partition(|key| ends[key].phase == Phase::Accepting(listener));
        listening.queue = queued;
        for key in accepting {
            self.abort(now, key);
        }
        Some(waker)
    }

    /// Removes end `key`, and resets its peer unless the peer reset it.
    fn abort(&mut self, now: Monotonic, key: u64) {
        let end = self.sockets.remove(self.wire, now, key);
        if end.broken.is_none() {
            let (pair, stream) = (end.pair, end.stream);
            self.sockets
                .lanes
                .send(self.wire, now, pair, stream, Kind::Rst(None));
        }
    }

    /// Takes the next stream that `listener` has, and gives the key and the pair of
    /// its end, or `EIO` when the listener failed and has none, or keeps `waker`. A
    /// listener that a power cut ended never has one. Returns the old waker, for the
    /// caller to drop after it releases the lock.
    pub(crate) fn accept(
        &mut self,
        listener: u64,
        waker: &task::Waker,
    ) -> (Poll<Accepted>, Option<Waker>) {
        let Some(listening) = self.sockets.listeners.get_mut(&listener) else {
            return (Poll::Pending, None);
        };
        let ends = &mut self.sockets.ends;
        let queued = (listening.queue.iter())
            .position(|key| ends[key].phase == Phase::Queued(listener));
        let Some(index) = queued else {
            if listening.failed {
                return (Poll::Ready(Err(Error::Io { code: EIO })), None);
            }
            return (
                Poll::Pending,
                listening.waker.replace(Waker::new(waker.clone())),
            );
        };
        let key = (listening.queue.remove(index)).expect("invariant: found");
        let end = (ends.get_mut(&key)).expect("invariant: queued");
        end.phase = Phase::Open;
        (Poll::Ready(Ok((key, end.pair))), None)
    }

    /// Connects from `node` to `remote`: sends a SYN from the next free port after
    /// the node's last connect, from 49152. Gives the key and the pair of the end.
    pub(crate) fn connect(
        &mut self,
        node: usize,
        now: Monotonic,
        remote: SocketAddr,
        options: tcp::Options,
    ) -> Result<(u64, Pair), Error> {
        let [v4, v6] = addresses(node);
        let ip = if remote.is_ipv4() { v4 } else { v6 };
        let sockets = &*self.sockets;
        let taken = |port: u16| {
            let own = |local: SocketAddr| {
                local.port() == port && addresses(node).contains(&local.ip())
            };
            sockets.routes.keys().any(|pair| own(pair.local))
                || (sockets.listeners.values()).any(|listening| {
                    listening.node == node && listening.local.port() == port
                })
        };
        let last = sockets.ports.get(&node).copied();
        let next = last
            .filter(|&port| port < u16::MAX)
            .map_or(EPHEMERAL, |port| port + 1);
        let port = (next..=u16::MAX)
            .chain(EPHEMERAL..next)
            .find(|&port| !taken(port));
        let Some(port) = port else {
            return Err(Error::Io {
                code: NOT_AVAILABLE,
            });
        };
        self.sockets.ports.insert(node, port);
        let pair = Pair {
            local: SocketAddr::new(ip, port),
            peer: remote,
        };
        let stream = self.sockets.key();
        let end = End::new(pair, stream, Phase::Connecting(now), options, 0);
        let edge = end.advertised;
        let key = self.sockets.insert(end);
        self.sockets.connects.insert((now, key));
        self.sockets
            .lanes
            .send(self.wire, now, pair, stream, Kind::Syn { edge });
        Ok((key, pair))
    }

    /// Whether the connect of `key` has ended, or keeps `waker`. Returns the old
    /// waker, for the caller to drop after it releases the lock.
    pub(crate) fn connected(
        &mut self,
        key: u64,
        waker: &task::Waker,
    ) -> (Poll<Result<(), Error>>, Option<Waker>) {
        let end = self.end(key);
        let remote = end.pair.peer;
        match (end.phase, end.broken) {
            (Phase::Connecting(_), None) => (
                Poll::Pending,
                end.writing.replace(Waker::new(waker.clone())),
            ),
            (Phase::Connecting(_), Some(Broken::Reset)) => {
                (Poll::Ready(Err(Error::Refused { remote })), None)
            }
            (Phase::Connecting(_), Some(Broken::TimedOut)) => {
                (Poll::Ready(Err(Error::TimedOut { remote })), None)
            }
            _ => (Poll::Ready(Ok(())), None),
        }
    }

    /// Reads into `buffer` from `key` at true time `now`, or keeps `waker`. Returns
    /// the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn read(
        &mut self,
        now: Monotonic,
        key: u64,
        waker: &task::Waker,
        buffer: &mut [u8],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let end = (self.sockets.ends.get_mut(&key))
            .expect("invariant: an end lives while its driver does");
        let mss = mss(self.wire, end.pair);
        if end.inbox.is_empty() {
            let poll = if let Some(broken) = end.broken {
                Poll::Ready(Err(end.error(broken)))
            } else if end.peer_closed {
                Poll::Ready(Ok(0))
            } else {
                return (
                    Poll::Pending,
                    end.reading.replace(Waker::new(waker.clone())),
                );
            };
            return (poll, None);
        }
        let n = buffer.len().min(end.inbox.len());
        for (to, from) in buffer.iter_mut().zip(end.inbox.drain(..n)) {
            *to = from;
        }
        end.read += n;
        let recv = end.options.recv_buffer_bytes;
        let edge = end.read.saturating_add(recv);
        // As on Linux, no update goes after the peer's FIN.
        let receiving = end.broken.is_none() && !end.peer_closed;
        if receiving && edge - end.advertised >= mss.min(recv / 2) {
            end.advertised = edge;
            let ack = end.ack();
            self.sockets
                .lanes
                .send(self.wire, now, end.pair, end.stream, ack);
        }
        (Poll::Ready(Ok(n)), None)
    }

    /// Writes from `buffers` to `key` at true time `now`, or keeps `waker`. Returns
    /// the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        key: u64,
        waker: &task::Waker,
        buffers: &[IoSlice<'_>],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let end = self.end(key);
        if let Some(broken) = end.broken {
            return (Poll::Ready(Err(end.error(broken))), None);
        }
        if end.fin != Fin::Unwritten {
            let code = BROKEN_PIPE;
            return (Poll::Ready(Err(Error::Io { code })), None);
        }
        let held = end.outbox.len() + (end.sent - end.acked);
        let room = end.options.send_buffer_bytes.saturating_sub(held);
        if end.outbox.len() >= end.options.unsent_bytes_max.get() || room == 0 {
            return (
                Poll::Pending,
                end.writing.replace(Waker::new(waker.clone())),
            );
        }
        let mut n = 0;
        for buffer in buffers {
            let take = buffer.len().min(room - n);
            end.outbox.extend(&buffer[..take]);
            n += take;
        }
        self.pump(now, key);
        (Poll::Ready(Ok(n)), None)
    }

    /// Closes `key` for writing at true time `now`: its FIN goes after the bytes not
    /// yet sent.
    pub(crate) fn close(&mut self, now: Monotonic, key: u64) -> Result<(), Error> {
        let end = self.end(key);
        if let Some(broken) = end.broken {
            return Err(end.error(broken));
        }
        if end.fin == Fin::Unwritten {
            end.fin = Fin::Queued;
            self.pump(now, key);
        }
        Ok(())
    }

    /// Drops the driver of `key` at true time `now`. Before its close, or with bytes
    /// unread, the peer of a stream that is not done gets an RST; after it, the end
    /// lives on as an orphan. Returns the end's wakers, for the caller to drop after
    /// it releases the lock.
    pub(crate) fn drop(&mut self, now: Monotonic, key: u64) -> [Option<Waker>; 2] {
        let end = self.end(key);
        let wakers = [end.reading.take(), end.writing.take()];
        let open = end.phase == Phase::Open && !end.done();
        if open && end.fin != Fin::Unwritten && end.inbox.is_empty() {
            end.phase = Phase::Orphan;
            return wakers;
        }
        let End { pair, stream, .. } = self.sockets.remove(self.wire, now, key);
        if open {
            self.sockets
                .lanes
                .send(self.wire, now, pair, stream, Kind::Rst(None));
        }
        wakers
    }

    /// Ends the streams and the listeners of `node`, whose power was cut at true time
    /// `now`, with no segment, after the wire dropped the segments that had not left
    /// the node. A connect that a driver holds is refused, and a stream that a driver
    /// holds is reset; the rest go. The peer of each stream that was not closed gets
    /// a lost RST, as [`Sockets::leave`] says. Returns their wakers, for the caller
    /// to drop after it releases the lock.
    pub(crate) fn cut_power(&mut self, now: Monotonic, node: usize) -> Vec<Waker> {
        self.sockets.lanes.cut(self.wire, node);
        let own = addresses(node);
        let ended: Vec<u64> = (self.sockets.ends.iter())
            .filter(|(_, end)| own.contains(&end.pair.local.ip()))
            .map(|(&key, _)| key)
            .collect();
        let mut wakers = Vec::new();
        self.sockets.listeners.retain(|_, listening| {
            let kept = listening.node != node;
            if !kept {
                wakers.extend(listening.waker.take());
            }
            kept
        });
        for key in ended {
            let end = self.end(key);
            wakers.extend(end.reading.take());
            wakers.extend(end.writing.take());
            match end.phase {
                Phase::Connecting(_) | Phase::Open => {}
                Phase::Accepting(_) | Phase::Queued(_) | Phase::Orphan => {
                    self.sockets.remove(self.wire, now, key);
                    continue;
                }
            }
            end.broken = Some(Broken::Reset);
            (end.inbox, end.outbox) = (VecDeque::new(), VecDeque::new());
            self.reap(now, key);
        }
        self.sockets.lanes.fall(node);
        wakers
    }

    /// Takes `segment`, which arrives at true time `at`. A segment other than a SYN
    /// that no end of its stream takes meets a closed port. Returns the wakers of the
    /// polls it makes ready.
    pub(super) fn arrive(&mut self, at: Monotonic, segment: Segment) -> Vec<Waker> {
        let Segment {
            source,
            destination,
            stream,
            kind,
            tries,
        } = segment;
        let carried = self.sockets.lanes.arrive(source, destination, stream);
        let tag = mem::discriminant(&kind);
        let len = kind.len();
        (self.wire).record((at, source, destination, tag, len, Fate::Arrived));
        let pair = Pair {
            local: destination,
            peer: source,
        };
        let mut found = self.find(pair, stream);
        let mut wakers = Vec::new();
        if let Some(Kind::Ack {
            received,
            edge,
            fin,
        }) = carried
            && let Some((key, true)) = found
        {
            wakers = self.ack(at, key, received, edge, fin);
            found = self.find(pair, stream);
        }
        let taken = self.take(at, pair, stream, kind, tries, found);
        if wakers.is_empty() {
            return taken;
        }
        wakers.extend(taken);
        wakers
    }

    /// The end of `stream` on `pair`, and whether it takes an ACK, bytes, or a FIN: a
    /// connecting end takes only its SYN-ACK and an RST.
    fn find(&self, pair: Pair, stream: u64) -> Option<(u64, bool)> {
        let key = *self.sockets.routes.get(&pair)?;
        let end = &self.sockets.ends[&key];
        let receives = !matches!(end.phase, Phase::Connecting(_));
        (end.stream == stream).then_some((key, receives))
    }

    /// Takes a segment of `kind` and `tries` of `stream` to `pair` at true time `at`,
    /// where `found` is its end as [`Self::find`] gives.
    fn take(
        &mut self,
        at: Monotonic,
        pair: Pair,
        stream: u64,
        kind: Kind,
        tries: Tries,
        found: Option<(u64, bool)>,
    ) -> Vec<Waker> {
        let end = found.map(|(key, _)| key);
        let receiver = found.filter(|&(_, receives)| receives).map(|(key, _)| key);
        match (kind, receiver) {
            (Kind::Syn { edge }, _) => {
                self.syn(at, pair, stream, edge, tries);
                Vec::new()
            }
            (Kind::SynAck { edge }, _) => {
                let connect = end.filter(|_| receiver.is_none());
                self.syn_ack(at, pair, stream, connect, edge, tries)
            }
            (Kind::Rst(_), _) => {
                end.map_or_else(Vec::new, |key| self.break_off(at, key, Broken::Reset))
            }
            (_, None) => {
                let lanes = &mut self.sockets.lanes;
                lanes.refuse(self.wire, at, pair, stream, Retry::Stream, tries);
                Vec::new()
            }
            (
                Kind::Ack {
                    received,
                    edge,
                    fin,
                },
                Some(key),
            ) => self.ack(at, key, received, edge, fin),
            (Kind::Data(bytes), Some(key)) => self.data(at, key, bytes, tries),
            (Kind::Fin, Some(key)) => self.fin(at, key),
        }
    }

    /// Takes a SYN of `stream` and `tries` to `pair` from its peer: a listener that
    /// receives it and has not failed answers, and otherwise an RST does.
    fn syn(
        &mut self,
        at: Monotonic,
        pair: Pair,
        stream: u64,
        edge: usize,
        tries: Tries,
    ) {
        if self.sockets.routes.contains_key(&pair) {
            self.sockets.lanes.yet.get_or_insert(REOPENED);
            return;
        }
        let Some(listener) = self.sockets.listener(pair.local) else {
            let lanes = &mut self.sockets.lanes;
            lanes.refuse(self.wire, at, pair, stream, Retry::Connect, tries);
            return;
        };
        let listening = &self.sockets.listeners[&listener];
        if listening.queue.len() >= listening.queue_max {
            self.sockets.lanes.yet.get_or_insert(BACKLOG);
            return;
        }
        let options = listening.options;
        let end = End::new(pair, stream, Phase::Accepting(listener), options, edge);
        let edge = end.advertised;
        let key = self.sockets.insert(end);
        let listening = self.sockets.listeners.get_mut(&listener);
        listening.expect("invariant: found").queue.push_back(key);
        self.sockets
            .lanes
            .send(self.wire, at, pair, stream, Kind::SynAck { edge });
    }

    /// Takes the SYN-ACK of `stream` and `tries` to `pair`, where `connect` is the end
    /// of the stream when it connects. With no connect there, an RST answers.
    fn syn_ack(
        &mut self,
        at: Monotonic,
        pair: Pair,
        stream: u64,
        connect: Option<u64>,
        edge: usize,
        tries: Tries,
    ) -> Vec<Waker> {
        let Some(key) = connect else {
            let lanes = &mut self.sockets.lanes;
            lanes.refuse(self.wire, at, pair, stream, Retry::Accept, tries);
            return Vec::new();
        };
        let end = self.end(key);
        let Phase::Connecting(since) = end.phase else {
            unreachable!("invariant: a connect connects");
        };
        (end.phase, end.edge) = (Phase::Open, edge);
        self.sockets.connects.remove(&(since, key));
        let end = self.end(key);
        let (ack, waker) = (end.ack(), end.writing.take());
        self.sockets.lanes.send(self.wire, at, pair, stream, ack);
        waker.into_iter().collect()
    }

    /// Ends the stream of `key` for `broken` at true time `now`, with no answer to
    /// its peer: an accepting end leaves its backlog, and another end fails its
    /// connect or drops its bytes not sent.
    fn break_off(&mut self, now: Monotonic, key: u64, broken: Broken) -> Vec<Waker> {
        let end = self.end(key);
        if let Phase::Accepting(listener) = end.phase {
            self.sockets.remove(self.wire, now, key);
            let listening = (self.sockets.listeners.get_mut(&listener))
                .expect("invariant: an accepting end has its listener");
            listening.queue.retain(|&queued| queued != key);
            return Vec::new();
        }
        end.broken = Some(broken);
        end.outbox.clear();
        let wakers = [end.reading.take(), end.writing.take()]
            .into_iter()
            .flatten()
            .collect();
        self.reap(now, key);
        wakers
    }

    /// Times out each connect [`CONNECT_TIMEOUT`] after its first SYN, as on Linux,
    /// then sends again each lost segment due by true time `now`, and times out each
    /// end that waits on a lost segment whose tries ended. Returns the wakers of the
    /// polls it makes ready.
    pub(super) fn retry(&mut self, now: Monotonic) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(&(since, key)) = self.sockets.connects.first()
            && (since.checked_add(CONNECT_TIMEOUT)).is_some_and(|at| at <= now)
        {
            self.sockets.connects.pop_first();
            wakers.extend(self.break_off(now, key, Broken::TimedOut));
        }
        let Sockets {
            ends,
            routes,
            lanes,
            ..
        } = &mut *self.sockets;
        let waits = |segment: &Segment| {
            owner(ends, routes, segment).map(|key| ends[&key].waits())
        };
        for segment in lanes.retry(self.wire, now, waits) {
            let Sockets { ends, routes, .. } = &*self.sockets;
            if let Some(key) = owner(ends, routes, &segment) {
                wakers.extend(self.break_off(now, key, Broken::TimedOut));
            }
        }
        wakers
    }

    /// Takes an ACK to `key`, a receiver.
    fn ack(
        &mut self,
        at: Monotonic,
        key: u64,
        received: usize,
        edge: usize,
        fin: bool,
    ) -> Vec<Waker> {
        let end = self.end(key);
        let mut wakers = Vec::new();
        if let Phase::Accepting(listener) = end.phase {
            end.phase = Phase::Queued(listener);
            let listening = (self.sockets.listeners.get_mut(&listener))
                .expect("invariant: an accepting end has its listener");
            wakers.extend(listening.waker.take());
        }
        let end = self.end(key);
        (end.acked, end.edge) = (received, edge);
        if fin && end.fin == Fin::Sent {
            end.fin = Fin::Acked;
        }
        wakers.extend(end.writing.take());
        self.pump(at, key);
        self.reap(at, key);
        wakers
    }

    /// Takes bytes of `tries` to `key`, a receiver. An orphan reads no more, so it
    /// resets.
    fn data(
        &mut self,
        at: Monotonic,
        key: u64,
        bytes: Vec<u8>,
        tries: Tries,
    ) -> Vec<Waker> {
        let end = self.end(key);
        if end.phase == Phase::Orphan {
            let (pair, stream) = (end.pair, end.stream);
            let lanes = &mut self.sockets.lanes;
            lanes.refuse(self.wire, at, pair, stream, Retry::Stream, tries);
            self.sockets.remove(self.wire, at, key);
            return Vec::new();
        }
        end.inbox.extend(bytes);
        let (pair, stream) = (end.pair, end.stream);
        let (ack, waker) = (end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, pair, stream, ack);
        waker.into_iter().collect()
    }

    /// Takes the FIN to `key`, a receiver.
    fn fin(&mut self, at: Monotonic, key: u64) -> Vec<Waker> {
        let end = self.end(key);
        end.peer_closed = true;
        let (pair, stream) = (end.pair, end.stream);
        let (ack, waker) = (end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, pair, stream, ack);
        self.reap(at, key);
        waker.into_iter().collect()
    }

    /// Sends the bytes of `key` that its peer's edge allows, then its FIN when it is
    /// queued and no byte is left.
    fn pump(&mut self, now: Monotonic, key: u64) {
        let end = (self.sockets.ends.get_mut(&key)).expect("invariant: the end lives");
        let mss = mss(self.wire, end.pair);
        while !end.outbox.is_empty() && end.sent < end.edge {
            let n = mss.min(end.outbox.len()).min(end.edge - end.sent);
            let bytes = end.outbox.drain(..n).collect();
            end.sent += n;
            self.sockets.lanes.send(
                self.wire,
                now,
                end.pair,
                end.stream,
                Kind::Data(bytes),
            );
        }
        if end.outbox.is_empty() && end.fin == Fin::Queued {
            end.fin = Fin::Sent;
            let (pair, stream) = (end.pair, end.stream);
            self.sockets
                .lanes
                .send(self.wire, now, pair, stream, Kind::Fin);
        }
        if end.waits() {
            self.sockets.lanes.arm(self.wire, now, end.pair, end.stream);
        }
    }

    /// Takes the segments to its pair from `key` once its stream is done at true
    /// time `now`, and removes it when it is also an orphan.
    fn reap(&mut self, now: Monotonic, key: u64) {
        let end = self.end(key);
        if !end.done() {
            return;
        }
        if end.phase == Phase::Orphan {
            self.sockets.remove(self.wire, now, key);
        } else {
            self.sockets.unroute(self.wire, now, key);
        }
    }

    fn end(&mut self, key: u64) -> &mut End {
        (self.sockets.ends.get_mut(&key)).expect("invariant: the end lives")
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use env::rng::Rng;

    use super::*;
    use crate::link;

    /// Takes the first segment in flight, and gives when it arrives.
    fn next(wire: &mut Wire) -> (Monotonic, Segment) {
        let at = wire.first().expect("a segment is in flight");
        let Some(Packet::Segment(segment)) = wire.pop(at) else {
            unreachable!("a segment is first");
        };
        (at, segment)
    }

    #[test]
    fn bytes_of_another_stream_on_the_pair_meet_a_closed_port() {
        let mut wire = Wire::new(link::Config::default(), Rng::from_seed(0));
        let mut sockets = Sockets::default();
        let [local, peer] = [0, 1].map(|node| SocketAddr::new(addresses(node)[0], 1));
        let options = tcp::Options {
            send_buffer_bytes: 1,
            recv_buffer_bytes: 1,
            unsent_bytes_max: NonZeroUsize::MIN,
            delayed: false,
        };
        let end = End::new(Pair { local, peer }, 2, Phase::Open, options, 1);
        let key = sockets.insert(end);
        let back = Pair {
            local: peer,
            peer: local,
        };
        (sockets.lanes).send(&mut wire, Monotonic(0), back, 1, Kind::Data(vec![7]));
        let (at, bytes) = next(&mut wire);
        Tcp::new(&mut sockets, &mut wire).arrive(at, bytes);
        assert!(sockets.ends[&key].inbox.is_empty());
        let (_, answer) = next(&mut wire);
        assert!(matches!(answer.kind, Kind::Rst(Some(Retry::Stream))));
        assert_eq!(answer.stream, 1);
    }

    #[test]
    fn a_direction_keeps_its_floor_only_while_a_segment_is_in_flight() {
        let mut wire = Wire::new(link::Config::default(), Rng::from_seed(0));
        let mut lanes = Lanes::default();
        let [local, peer] = [0, 1].map(|node| SocketAddr::new(addresses(node)[0], 1));
        for _ in 0..2 {
            lanes.send(&mut wire, Monotonic(0), Pair { local, peer }, 1, Kind::Fin);
        }
        lanes.arrive(local, peer, 1);
        assert_eq!(lanes.floors.len(), 1);
        lanes.arrive(local, peer, 1);
        assert!(lanes.floors.is_empty());
    }

    #[test]
    fn a_read_from_a_reset_end_sends_nothing() {
        let mut wire = Wire::new(link::Config::default(), Rng::from_seed(0));
        let mut sockets = Sockets::default();
        let [local, peer] = [0, 1].map(|node| SocketAddr::new(addresses(node)[0], 1));
        let options = tcp::Options {
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
            delayed: false,
        };
        let mut end = End::new(Pair { local, peer }, 1, Phase::Open, options, 0);
        end.inbox.extend([0; 4_096]);
        let key = sockets.insert(end);
        let mut buffer = [0; 4_096];
        let mut tcp = Tcp::new(&mut sockets, &mut wire);
        tcp.break_off(Monotonic(0), key, Broken::Reset);
        let (poll, _) = tcp.read(Monotonic(0), key, task::Waker::noop(), &mut buffer);
        assert!(matches!(poll, Poll::Ready(Ok(4_096))));
        assert!(sockets.lanes.floors.is_empty());
    }

    #[test]
    fn removing_an_old_end_keeps_the_route_of_a_newer_end_on_its_pair() {
        let [local, peer] = [0, 1].map(|node| SocketAddr::new(addresses(node)[0], 1));
        let pair = Pair { local, peer };
        let options = tcp::Options {
            send_buffer_bytes: 1,
            recv_buffer_bytes: 1,
            unsent_bytes_max: NonZeroUsize::MIN,
            delayed: false,
        };
        let mut wire = Wire::new(link::Config::default(), Rng::from_seed(0));
        let mut sockets = Sockets::default();
        let old = sockets.insert(End::new(pair, 1, Phase::Open, options, 1));
        let tcp = &mut Tcp::new(&mut sockets, &mut wire);
        tcp.break_off(Monotonic(0), old, Broken::Reset);
        let new = sockets.insert(End::new(pair, 2, Phase::Open, options, 1));
        sockets.remove(&wire, Monotonic(0), old);
        assert_eq!(sockets.routes.get(&pair), Some(&new));
    }
}
