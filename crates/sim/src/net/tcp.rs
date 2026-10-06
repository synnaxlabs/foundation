//! TCP streams and listeners. A segment is never lost or duplicated, and the
//! segments of one direction of a stream arrive in the order they were sent.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::io::IoSlice;
use std::mem;
use std::net::SocketAddr;
use std::task::{Poll, Waker};

use env::net::{Error, tcp};
use types::time::Monotonic;

use super::wire::{Packet, Wire};
use super::{EPHEMERAL, Fate, NOT_AVAILABLE, addresses, node, receives};

/// The IPv4 and TCP header bytes of a segment.
const V4_HEADERS: usize = 40;
/// The IPv6 and TCP header bytes of a segment.
const V6_HEADERS: usize = 60;
/// The least segment size, as Linux clamps it (`tcp_min_snd_mss`).
const MSS_MIN: usize = 48;
/// The Linux code for a write after a close (`EPIPE`).
const BROKEN_PIPE: i32 = 32;

const LOSSY: &str = "sim does not simulate TCP on a lossy link yet";
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
    kind: Kind,
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
    Rst,
}

impl Kind {
    /// The bytes it carries.
    fn len(&self) -> usize {
        match self {
            Self::Data(bytes) => bytes.len(),
            _ => 0,
        }
    }
}

/// Where an end is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Its SYN is out, and its connect waits.
    Connecting,
    /// An RST answered its SYN.
    Refused,
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
    phase: Phase,
    options: tcp::Options,
    /// The bytes that arrived and were not read.
    inbox: VecDeque<u8>,
    read: usize,
    /// The edge in the last segment to the peer.
    advertised: usize,
    /// The peer's FIN arrived.
    peer_closed: bool,
    /// An RST arrived. The end sends nothing more and takes no segment.
    reset: bool,
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
    fn new(pair: Pair, phase: Phase, options: tcp::Options, edge: usize) -> Self {
        Self {
            pair,
            phase,
            options,
            inbox: VecDeque::new(),
            read: 0,
            advertised: options.recv_buffer_bytes,
            peer_closed: false,
            reset: false,
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

    /// Whether the stream has ended: an RST arrived or answered its SYN, or each FIN
    /// arrived and its own is acked. As on Linux, a SYN to its pair then opens a new
    /// stream, and a drop of it sends nothing.
    fn done(&self) -> bool {
        self.reset
            || self.phase == Phase::Refused
            || (self.fin == Fin::Acked && self.peer_closed)
    }

    /// Whether an orphan has nothing left to do.
    fn finished(&self) -> bool {
        self.phase == Phase::Orphan && self.done()
    }

    /// Whether the end takes a segment other than a SYN. One that does not answers
    /// with an RST, as a closed port does.
    fn live(&self) -> bool {
        !self.reset && !matches!(self.phase, Phase::Connecting | Phase::Refused)
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
}

/// The order of the segments in flight in each direction, and the first case that
/// sim does not simulate yet.
#[derive(Default)]
struct Lanes {
    /// The latest arrival in flight in each direction, by source and destination,
    /// and the count of segments in flight there.
    floors: BTreeMap<(SocketAddr, SocketAddr), (Monotonic, usize)>,
    yet: Option<&'static str>,
}

impl Lanes {
    /// Puts a segment of `kind` from end `from` in flight at true time `now`. It
    /// arrives after the delay and a jitter draw of its link, and not before a
    /// segment in flight in its direction.
    fn send(&mut self, wire: &mut Wire, now: Monotonic, from: Pair, kind: Kind) {
        let node = node(from.local.ip()).expect("invariant: an end is on a node");
        let link = wire.path(node, from.peer.ip());
        if link.loss > 0.0 {
            self.yet.get_or_insert(LOSSY);
        }
        let (source, destination) = (from.local, from.peer);
        let tag = mem::discriminant(&kind);
        wire.record((now, source, destination, tag, kind.len(), Fate::Sent));
        let Some(at) = wire.draw(now, &link) else {
            return;
        };
        let floor = self.floors.entry((source, destination)).or_insert((at, 0));
        floor.0 = floor.0.max(at);
        floor.1 += 1;
        let segment = Segment {
            source,
            destination,
            kind,
        };
        wire.put(floor.0, Packet::Segment(segment));
    }

    /// Notes the arrival of a segment from `source` to `destination`.
    fn arrive(&mut self, source: SocketAddr, destination: SocketAddr) {
        let Entry::Occupied(mut floor) = self.floors.entry((source, destination))
        else {
            panic!("invariant: a segment in flight has its floor");
        };
        floor.get_mut().1 -= 1;
        if floor.get().1 == 0 {
            floor.remove();
        }
    }
}

/// The TCP ends and listeners of a run.
#[derive(Default)]
pub(super) struct Sockets {
    ends: BTreeMap<u64, End>,
    /// The key of the end that takes the segments to each pair.
    routes: BTreeMap<Pair, u64>,
    listeners: BTreeMap<u64, Listening>,
    /// The port of the last connect of each node.
    ports: BTreeMap<usize, u16>,
    /// The last key given to an end or a listener.
    last: u64,
    lanes: Lanes,
}

impl Sockets {
    /// Takes the first case met that sim does not simulate yet.
    pub(super) fn yet(&mut self) -> Option<&'static str> {
        self.lanes.yet.take()
    }

    /// A key that no end or listener has had.
    fn key(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    /// Adds `end`, which takes the segments to its pair from an end that is done,
    /// and gives its key.
    fn insert(&mut self, end: End) -> u64 {
        let key = self.key();
        let old = self.routes.insert(end.pair, key);
        let done = old.is_none_or(|old| self.ends[&old].done());
        assert!(done, "invariant: a pair has one end that is not done");
        self.ends.insert(key, end);
        key
    }

    /// Removes end `key`, and the route of its pair when a newer end does not have
    /// it.
    fn remove(&mut self, key: u64) -> End {
        let end = (self.ends.remove(&key)).expect("invariant: the end lives");
        match self.routes.entry(end.pair) {
            Entry::Occupied(route) if *route.get() == key => {
                route.remove();
            }
            _ => assert!(
                end.done(),
                "invariant: only an end that is done loses its pair"
            ),
        }
        end
    }
}

/// The TCP ends and listeners of a run, with the wire they send on.
pub(crate) struct Tcp<'a> {
    sockets: &'a mut Sockets,
    wire: &'a mut Wire,
}

/// The largest data segment from `from` on its link.
fn mss(wire: &Wire, from: Pair) -> usize {
    let node = node(from.local.ip()).expect("invariant: an end is on a node");
    let header = if from.peer.is_ipv4() {
        V4_HEADERS
    } else {
        V6_HEADERS
    };
    let mtu = wire.path(node, from.peer.ip()).mtu;
    mtu.saturating_sub(header).max(MSS_MIN)
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
            let end = self.sockets.remove(key);
            if !end.reset {
                self.sockets.lanes.send(self.wire, now, end.pair, Kind::Rst);
            }
        }
        listening.waker
    }

    /// Takes the next stream that `listener` has, and gives the key and the pair of
    /// its end, or keeps `waker`. A listener that a power cut ended never has one.
    /// Returns the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn accept(
        &mut self,
        listener: u64,
        waker: &Waker,
    ) -> (Poll<(u64, Pair)>, Option<Waker>) {
        let Some(listening) = self.sockets.listeners.get_mut(&listener) else {
            return (Poll::Pending, None);
        };
        let ends = &mut self.sockets.ends;
        let queued = (listening.queue.iter())
            .position(|key| ends[key].phase == Phase::Queued(listener));
        let Some(index) = queued else {
            return (Poll::Pending, listening.waker.replace(waker.clone()));
        };
        let key = (listening.queue.remove(index)).expect("invariant: found");
        let end = (ends.get_mut(&key)).expect("invariant: queued");
        end.phase = Phase::Open;
        (Poll::Ready((key, end.pair)), None)
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
            sockets.ends.values().any(|end| own(end.pair.local))
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
        let end = End::new(pair, Phase::Connecting, options, 0);
        let edge = end.advertised;
        let key = self.sockets.insert(end);
        self.sockets
            .lanes
            .send(self.wire, now, pair, Kind::Syn { edge });
        Ok((key, pair))
    }

    /// Whether the connect of `key` has ended, or keeps `waker`. Returns the old
    /// waker, for the caller to drop after it releases the lock.
    pub(crate) fn connected(
        &mut self,
        key: u64,
        waker: &Waker,
    ) -> (Poll<Result<(), Error>>, Option<Waker>) {
        let end = self.end(key);
        match end.phase {
            Phase::Connecting => (Poll::Pending, end.writing.replace(waker.clone())),
            Phase::Refused => {
                let remote = end.pair.peer;
                (Poll::Ready(Err(Error::Refused { remote })), None)
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
        waker: &Waker,
        buffer: &mut [u8],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let end = (self.sockets.ends.get_mut(&key))
            .expect("invariant: an end lives while its driver does");
        let mss = mss(self.wire, end.pair);
        if end.inbox.is_empty() {
            let poll = if end.reset {
                Poll::Ready(Err(Error::Reset {
                    remote: end.pair.peer,
                }))
            } else if end.peer_closed {
                Poll::Ready(Ok(0))
            } else {
                return (Poll::Pending, end.reading.replace(waker.clone()));
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
        let receiving = !end.reset && !end.peer_closed;
        if receiving && edge - end.advertised >= mss.min(recv / 2) {
            end.advertised = edge;
            let ack = end.ack();
            self.sockets.lanes.send(self.wire, now, end.pair, ack);
        }
        (Poll::Ready(Ok(n)), None)
    }

    /// Writes from `buffers` to `key` at true time `now`, or keeps `waker`. Returns
    /// the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        key: u64,
        waker: &Waker,
        buffers: &[IoSlice<'_>],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let end = self.end(key);
        if end.reset {
            let remote = end.pair.peer;
            return (Poll::Ready(Err(Error::Reset { remote })), None);
        }
        if end.fin != Fin::Unwritten {
            let code = BROKEN_PIPE;
            return (Poll::Ready(Err(Error::Io { code })), None);
        }
        let held = end.outbox.len() + (end.sent - end.acked);
        let room = end.options.send_buffer_bytes.saturating_sub(held);
        if end.outbox.len() >= end.options.unsent_bytes_max || room == 0 {
            return (Poll::Pending, end.writing.replace(waker.clone()));
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
        if end.reset {
            return Err(Error::Reset {
                remote: end.pair.peer,
            });
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
        let pair = self.sockets.remove(key).pair;
        if open {
            self.sockets.lanes.send(self.wire, now, pair, Kind::Rst);
        }
        wakers
    }

    /// Ends the streams and the listeners of `node`, whose power was cut, with no
    /// segment. A connect that a driver holds is refused, and a stream that a driver
    /// holds is reset; the rest go. Returns their wakers, for the caller to drop after
    /// it releases the lock.
    pub(crate) fn cut_power(&mut self, node: usize) -> Vec<Waker> {
        let own = addresses(node);
        let ended: Vec<u64> = (self.sockets.ends.iter())
            .filter(|(_, end)| own.contains(&end.pair.local.ip()))
            .map(|(&key, _)| key)
            .collect();
        let mut wakers = Vec::new();
        for key in ended {
            let end = self.end(key);
            wakers.extend(end.reading.take());
            wakers.extend(end.writing.take());
            match end.phase {
                Phase::Connecting => end.phase = Phase::Refused,
                Phase::Refused | Phase::Open => {}
                Phase::Accepting(_) | Phase::Queued(_) | Phase::Orphan => {
                    self.sockets.remove(key);
                    continue;
                }
            }
            end.reset = true;
            (end.inbox, end.outbox) = (VecDeque::new(), VecDeque::new());
        }
        self.sockets.listeners.retain(|_, listening| {
            let kept = listening.node != node;
            if !kept {
                wakers.extend(listening.waker.take());
            }
            kept
        });
        wakers
    }

    /// Takes `segment`, which arrives at true time `at`. Returns the wakers of the
    /// polls it makes ready.
    pub(super) fn arrive(&mut self, at: Monotonic, segment: Segment) -> Vec<Waker> {
        let Segment {
            source,
            destination,
            kind,
        } = segment;
        self.sockets.lanes.arrive(source, destination);
        let tag = mem::discriminant(&kind);
        let len = kind.len();
        (self.wire).record((at, source, destination, tag, len, Fate::Arrived));
        let pair = Pair {
            local: destination,
            peer: source,
        };
        let route = self.sockets.routes.get(&pair).copied();
        let receiver = route.filter(|&key| self.end(key).live());
        match (kind, receiver) {
            (Kind::Syn { edge }, _) => {
                self.syn(at, pair, route, edge);
                Vec::new()
            }
            (Kind::SynAck { edge }, _) => self.syn_ack(at, pair, route, edge),
            (Kind::Rst, _) => route.map_or_else(Vec::new, |key| self.rst(key)),
            (_, None) => {
                self.sockets.lanes.send(self.wire, at, pair, Kind::Rst);
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
            (Kind::Data(bytes), Some(key)) => self.data(at, key, bytes),
            (Kind::Fin, Some(key)) => self.fin(at, key),
        }
    }

    /// Takes a SYN to `pair` from its peer, which `route` has: a listener that
    /// receives it answers, and otherwise an RST does. An end that is done on the
    /// pair does not take it.
    fn syn(&mut self, at: Monotonic, pair: Pair, route: Option<u64>, edge: usize) {
        if route.is_some_and(|key| !self.end(key).done()) {
            self.sockets.lanes.yet.get_or_insert(REOPENED);
            return;
        }
        let listener = (self.sockets.listeners.iter_mut()).find(|(_, listening)| {
            receives(listening.node, listening.local, pair.local)
        });
        let Some((&listener, listening)) = listener else {
            self.sockets.lanes.send(self.wire, at, pair, Kind::Rst);
            return;
        };
        if listening.queue.len() >= listening.queue_max {
            self.sockets.lanes.yet.get_or_insert(BACKLOG);
            return;
        }
        let end = End::new(pair, Phase::Accepting(listener), listening.options, edge);
        let edge = end.advertised;
        let key = self.sockets.insert(end);
        let listening = self.sockets.listeners.get_mut(&listener);
        listening.expect("invariant: found").queue.push_back(key);
        self.sockets
            .lanes
            .send(self.wire, at, pair, Kind::SynAck { edge });
    }

    /// Takes the SYN-ACK of the connect of `pair`, which `route` has. With no
    /// connect there, an RST answers.
    fn syn_ack(
        &mut self,
        at: Monotonic,
        pair: Pair,
        route: Option<u64>,
        edge: usize,
    ) -> Vec<Waker> {
        let key = route.filter(|&key| self.end(key).phase == Phase::Connecting);
        let Some(key) = key else {
            self.sockets.lanes.send(self.wire, at, pair, Kind::Rst);
            return Vec::new();
        };
        let end = self.end(key);
        (end.phase, end.edge) = (Phase::Open, edge);
        let (ack, waker) = (end.ack(), end.writing.take());
        self.sockets.lanes.send(self.wire, at, pair, ack);
        waker.into_iter().collect()
    }

    /// Takes an RST to `key`. An RST never gets an answer.
    fn rst(&mut self, key: u64) -> Vec<Waker> {
        let end = self.end(key);
        match end.phase {
            Phase::Connecting => {
                end.phase = Phase::Refused;
                end.writing.take().into_iter().collect()
            }
            Phase::Accepting(listener) => {
                self.sockets.remove(key);
                let listening = (self.sockets.listeners.get_mut(&listener))
                    .expect("invariant: an accepting end has its listener");
                listening.queue.retain(|&queued| queued != key);
                Vec::new()
            }
            Phase::Orphan => {
                self.sockets.remove(key);
                Vec::new()
            }
            Phase::Refused | Phase::Queued(_) | Phase::Open => {
                end.reset = true;
                end.outbox.clear();
                [end.reading.take(), end.writing.take()]
                    .into_iter()
                    .flatten()
                    .collect()
            }
        }
    }

    /// Takes an ACK to `key`, a live end.
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
        self.reap(key);
        wakers
    }

    /// Takes bytes to `key`, a live end. An orphan reads no more, so it resets.
    fn data(&mut self, at: Monotonic, key: u64, bytes: Vec<u8>) -> Vec<Waker> {
        let end = self.end(key);
        if end.phase == Phase::Orphan {
            let pair = self.sockets.remove(key).pair;
            self.sockets.lanes.send(self.wire, at, pair, Kind::Rst);
            return Vec::new();
        }
        end.inbox.extend(bytes);
        let (pair, ack, waker) = (end.pair, end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, pair, ack);
        waker.into_iter().collect()
    }

    /// Takes the FIN to `key`, a live end.
    fn fin(&mut self, at: Monotonic, key: u64) -> Vec<Waker> {
        let end = self.end(key);
        end.peer_closed = true;
        let (pair, ack, waker) = (end.pair, end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, pair, ack);
        self.reap(key);
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
            self.sockets
                .lanes
                .send(self.wire, now, end.pair, Kind::Data(bytes));
        }
        if end.outbox.is_empty() && end.fin == Fin::Queued {
            end.fin = Fin::Sent;
            self.sockets.lanes.send(self.wire, now, end.pair, Kind::Fin);
        }
    }

    /// Removes `key` when it is an orphan with nothing left to do.
    fn reap(&mut self, key: u64) {
        if self.end(key).finished() {
            self.sockets.remove(key);
        }
    }

    fn end(&mut self, key: u64) -> &mut End {
        (self.sockets.ends.get_mut(&key)).expect("invariant: the end lives")
    }
}

#[cfg(test)]
mod tests {
    use env::rng::Rng;

    use super::*;
    use crate::link;

    #[test]
    fn a_direction_keeps_its_floor_only_while_a_segment_is_in_flight() {
        let mut wire = Wire::new(link::Config::default(), Rng::from_seed(0));
        let mut lanes = Lanes::default();
        let [local, peer] = [0, 1].map(|node| SocketAddr::new(addresses(node)[0], 1));
        for _ in 0..2 {
            lanes.send(&mut wire, Monotonic(0), Pair { local, peer }, Kind::Fin);
        }
        lanes.arrive(local, peer);
        assert_eq!(lanes.floors.len(), 1);
        lanes.arrive(local, peer);
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
            unsent_bytes_max: 1 << 14,
            delayed: false,
        };
        let mut end = End::new(Pair { local, peer }, Phase::Open, options, 0);
        end.reset = true;
        end.inbox.extend([0; 4_096]);
        let key = sockets.insert(end);
        let mut buffer = [0; 4_096];
        let mut tcp = Tcp::new(&mut sockets, &mut wire);
        let (poll, _) = tcp.read(Monotonic(0), key, Waker::noop(), &mut buffer);
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
            unsent_bytes_max: 1,
            delayed: false,
        };
        let mut sockets = Sockets::default();
        let old = sockets.insert(End::new(pair, Phase::Open, options, 1));
        sockets.ends.get_mut(&old).unwrap().reset = true;
        let new = sockets.insert(End::new(pair, Phase::Open, options, 1));
        sockets.remove(old);
        assert_eq!(sockets.routes.get(&pair), Some(&new));
    }
}
