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
use super::{EPHEMERAL, Fate, NOT_AVAILABLE, addresses, covers, node};

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

/// One end of a stream, by its address and its peer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
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
    fn new(phase: Phase, options: tcp::Options, edge: usize) -> Self {
        Self {
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

    /// Whether an orphan has nothing left to do.
    fn finished(&self) -> bool {
        self.phase == Phase::Orphan && self.fin == Fin::Acked && self.peer_closed
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
    /// Its ends in [`Phase::Accepting`] and [`Phase::Queued`], in the order of their
    /// SYNs.
    queue: VecDeque<Key>,
    waker: Option<Waker>,
}

impl Listening {
    /// Whether a SYN to `destination` comes to it.
    fn receives(&self, destination: SocketAddr) -> bool {
        let (local, ip) = (self.local.ip(), destination.ip());
        self.local.port() == destination.port()
            && covers(local, ip)
            && node(ip) == Some(self.node)
    }
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
    fn send(&mut self, wire: &mut Wire, now: Monotonic, from: Key, kind: Kind) {
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
        if let Entry::Occupied(mut floor) = self.floors.entry((source, destination)) {
            floor.get_mut().1 -= 1;
            if floor.get().1 == 0 {
                floor.remove();
            }
        }
    }
}

/// The TCP ends and listeners of a run.
#[derive(Default)]
pub(super) struct Sockets {
    ends: BTreeMap<Key, End>,
    listeners: BTreeMap<u64, Listening>,
    /// The port of the last connect of each node.
    ports: BTreeMap<usize, u16>,
    next: u64,
    lanes: Lanes,
}

impl Sockets {
    /// Takes the first case met that sim does not simulate yet.
    pub(super) fn yet(&mut self) -> Option<&'static str> {
        self.lanes.yet.take()
    }
}

/// The TCP ends and listeners of a run, with the wire they send on.
pub(crate) struct Tcp<'a> {
    sockets: &'a mut Sockets,
    wire: &'a mut Wire,
}

/// The largest data segment from `from` on its link.
fn mss(wire: &Wire, from: Key) -> usize {
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
        let local = config.local;
        let ip = local.ip();
        if !ip.is_unspecified() && !addresses(node).contains(&ip) {
            return Err(Error::Io {
                code: NOT_AVAILABLE,
            });
        }
        let taken = |port: u16| {
            (self.sockets.listeners.values()).any(|listening| {
                let other = listening.local.ip();
                listening.node == node
                    && listening.local.port() == port
                    && (covers(ip, other) || covers(other, ip))
            })
        };
        let port = match local.port() {
            0 => (EPHEMERAL..=u16::MAX).find(|&port| !taken(port)),
            port => Some(port).filter(|&port| !taken(port)),
        };
        let Some(port) = port else {
            return Err(Error::AddressInUse { local });
        };
        let local = SocketAddr::new(ip, port);
        let backlog = usize::try_from(config.backlog).unwrap_or(usize::MAX);
        let listening = Listening {
            node,
            local,
            queue_max: backlog.saturating_add(1),
            options: config.options,
            queue: VecDeque::new(),
            waker: None,
        };
        self.sockets.next += 1;
        let key = self.sockets.next;
        self.sockets.listeners.insert(key, listening);
        Ok((key, local))
    }

    /// Stops listener `key`, and resets the streams that it has not accepted. A
    /// listener that a power cut ended is gone already. Returns the waker of its
    /// accept, for the caller to drop after it releases the lock.
    pub(crate) fn unlisten(&mut self, now: Monotonic, key: u64) -> Option<Waker> {
        let listening = self.sockets.listeners.remove(&key)?;
        for end in listening.queue {
            let reset = self.sockets.ends.remove(&end).is_some_and(|end| !end.reset);
            if reset {
                self.sockets.lanes.send(self.wire, now, end, Kind::Rst);
            }
        }
        listening.waker
    }

    /// Takes the next stream that listener `key` has, or keeps `waker`. A listener
    /// that a power cut ended never has one. Returns the old waker, for the caller to
    /// drop after it releases the lock.
    pub(crate) fn accept(
        &mut self,
        key: u64,
        waker: &Waker,
    ) -> (Poll<Key>, Option<Waker>) {
        let Some(listening) = self.sockets.listeners.get_mut(&key) else {
            return (Poll::Pending, None);
        };
        let ends = &mut self.sockets.ends;
        let queued = (listening.queue.iter())
            .position(|end| ends[end].phase == Phase::Queued(key));
        let Some(index) = queued else {
            return (Poll::Pending, listening.waker.replace(waker.clone()));
        };
        let end = (listening.queue.remove(index)).expect("invariant: found");
        (ends.get_mut(&end).expect("invariant: queued")).phase = Phase::Open;
        (Poll::Ready(end), None)
    }

    /// Connects from `node` to `remote`: sends a SYN from the next free port after
    /// the node's last connect, from 49152.
    pub(crate) fn connect(
        &mut self,
        node: usize,
        now: Monotonic,
        remote: SocketAddr,
        options: tcp::Options,
    ) -> Result<Key, Error> {
        let [v4, v6] = addresses(node);
        let ip = if remote.is_ipv4() { v4 } else { v6 };
        let sockets = &*self.sockets;
        let taken = |port: u16| {
            let own = |local: SocketAddr| {
                local.port() == port && addresses(node).contains(&local.ip())
            };
            sockets.ends.keys().any(|key| own(key.local))
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
        let key = Key {
            local: SocketAddr::new(ip, port),
            peer: remote,
        };
        let end = End::new(Phase::Connecting, options, 0);
        let edge = end.advertised;
        self.sockets.ends.insert(key, end);
        self.sockets
            .lanes
            .send(self.wire, now, key, Kind::Syn { edge });
        Ok(key)
    }

    /// Whether the connect of `key` has ended, or keeps `waker`. Returns the old
    /// waker, for the caller to drop after it releases the lock.
    pub(crate) fn connected(
        &mut self,
        key: Key,
        waker: &Waker,
    ) -> (Poll<Result<(), Error>>, Option<Waker>) {
        let end = self.end(key);
        match end.phase {
            Phase::Connecting => (Poll::Pending, end.writing.replace(waker.clone())),
            Phase::Refused => {
                (Poll::Ready(Err(Error::Refused { remote: key.peer })), None)
            }
            _ => (Poll::Ready(Ok(())), None),
        }
    }

    /// Reads into `buffer` from `key` at true time `now`, or keeps `waker`. Returns
    /// the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn read(
        &mut self,
        now: Monotonic,
        key: Key,
        waker: &Waker,
        buffer: &mut [u8],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let mss = mss(self.wire, key);
        let end = (self.sockets.ends.get_mut(&key))
            .expect("invariant: an end lives while its driver does");
        if end.inbox.is_empty() {
            let poll = if end.reset {
                Poll::Ready(Err(Error::Reset { remote: key.peer }))
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
        let edge = end.read + recv;
        if !end.reset && edge - end.advertised >= mss.min(recv / 2) {
            end.advertised = edge;
            let ack = end.ack();
            self.sockets.lanes.send(self.wire, now, key, ack);
        }
        (Poll::Ready(Ok(n)), None)
    }

    /// Writes from `buffers` to `key` at true time `now`, or keeps `waker`. Returns
    /// the old waker, for the caller to drop after it releases the lock.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        key: Key,
        waker: &Waker,
        buffers: &[IoSlice<'_>],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let end = self.end(key);
        if end.reset {
            return (Poll::Ready(Err(Error::Reset { remote: key.peer })), None);
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
    pub(crate) fn close(&mut self, now: Monotonic, key: Key) -> Result<(), Error> {
        let end = self.end(key);
        if end.reset {
            return Err(Error::Reset { remote: key.peer });
        }
        if end.fin == Fin::Unwritten {
            end.fin = Fin::Queued;
            self.pump(now, key);
        }
        Ok(())
    }

    /// Drops the driver of `key` at true time `now`. Before its close, or with bytes
    /// unread, the peer gets an RST; after it, the end lives on as an orphan. Returns
    /// the end's wakers, for the caller to drop after it releases the lock.
    pub(crate) fn drop(&mut self, now: Monotonic, key: Key) -> [Option<Waker>; 2] {
        let Entry::Occupied(mut entry) = self.sockets.ends.entry(key) else {
            return [None, None];
        };
        let end = entry.get_mut();
        let wakers = [end.reading.take(), end.writing.take()];
        let open = end.phase == Phase::Open && !end.reset;
        if open && end.fin != Fin::Unwritten && end.inbox.is_empty() {
            end.phase = Phase::Orphan;
            if end.finished() {
                entry.remove();
            }
            return wakers;
        }
        entry.remove();
        if open {
            self.sockets.lanes.send(self.wire, now, key, Kind::Rst);
        }
        wakers
    }

    /// Ends the streams and the listeners of `node`, whose power was cut, with no
    /// segment. A connect that a driver holds is refused, and a stream that a driver
    /// holds is reset; the rest go. Returns their wakers, for the caller to drop after
    /// it releases the lock.
    pub(crate) fn cut_power(&mut self, node: usize) -> Vec<Waker> {
        let own = addresses(node);
        let mut wakers = Vec::new();
        self.sockets.ends.retain(|key, end| {
            if !own.contains(&key.local.ip()) {
                return true;
            }
            wakers.extend(end.reading.take());
            wakers.extend(end.writing.take());
            match end.phase {
                Phase::Connecting => end.phase = Phase::Refused,
                Phase::Refused | Phase::Open => {}
                Phase::Accepting(_) | Phase::Queued(_) | Phase::Orphan => return false,
            }
            end.reset = true;
            (end.inbox, end.outbox) = (VecDeque::new(), VecDeque::new());
            true
        });
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
        let key = Key {
            local: destination,
            peer: source,
        };
        let live = self.sockets.ends.get(&key).is_some_and(End::live);
        match kind {
            Kind::Syn { edge } => {
                self.syn(at, key, edge);
                Vec::new()
            }
            Kind::SynAck { edge } => self.syn_ack(at, key, edge),
            Kind::Rst => self.rst(key),
            _ if !live => {
                self.sockets.lanes.send(self.wire, at, key, Kind::Rst);
                Vec::new()
            }
            Kind::Ack {
                received,
                edge,
                fin,
            } => self.ack(at, key, received, edge, fin),
            Kind::Data(bytes) => self.data(at, key, bytes),
            Kind::Fin => self.fin(at, key),
        }
    }

    /// Takes a SYN to `key` from its peer: a listener that receives it answers, and
    /// otherwise an RST does.
    fn syn(&mut self, at: Monotonic, key: Key, edge: usize) {
        if self.sockets.ends.contains_key(&key) {
            self.sockets.lanes.yet.get_or_insert(REOPENED);
            return;
        }
        let listener = (self.sockets.listeners.iter_mut())
            .find(|(_, listening)| listening.receives(key.local));
        let Some((&listener, listening)) = listener else {
            self.sockets.lanes.send(self.wire, at, key, Kind::Rst);
            return;
        };
        if listening.queue.len() >= listening.queue_max {
            self.sockets.lanes.yet.get_or_insert(BACKLOG);
            return;
        }
        listening.queue.push_back(key);
        let end = End::new(Phase::Accepting(listener), listening.options, edge);
        let edge = end.advertised;
        self.sockets.ends.insert(key, end);
        self.sockets
            .lanes
            .send(self.wire, at, key, Kind::SynAck { edge });
    }

    /// Takes the SYN-ACK of the connect of `key`. With no connect there, an RST
    /// answers.
    fn syn_ack(&mut self, at: Monotonic, key: Key, edge: usize) -> Vec<Waker> {
        let end = (self.sockets.ends.get_mut(&key))
            .filter(|end| end.phase == Phase::Connecting);
        let Some(end) = end else {
            self.sockets.lanes.send(self.wire, at, key, Kind::Rst);
            return Vec::new();
        };
        (end.phase, end.edge) = (Phase::Open, edge);
        let (ack, waker) = (end.ack(), end.writing.take());
        self.sockets.lanes.send(self.wire, at, key, ack);
        waker.into_iter().collect()
    }

    /// Takes an RST to `key`. An RST never gets an answer.
    fn rst(&mut self, key: Key) -> Vec<Waker> {
        let Entry::Occupied(mut entry) = self.sockets.ends.entry(key) else {
            return Vec::new();
        };
        let end = entry.get_mut();
        match end.phase {
            Phase::Connecting => {
                end.phase = Phase::Refused;
                end.writing.take().into_iter().collect()
            }
            Phase::Accepting(listener) => {
                entry.remove();
                let listening = (self.sockets.listeners.get_mut(&listener))
                    .expect("invariant: an accepting end has its listener");
                listening.queue.retain(|&queued| queued != key);
                Vec::new()
            }
            Phase::Orphan => {
                entry.remove();
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
        key: Key,
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
    fn data(&mut self, at: Monotonic, key: Key, bytes: Vec<u8>) -> Vec<Waker> {
        let end = self.end(key);
        if end.phase == Phase::Orphan {
            self.sockets.ends.remove(&key);
            self.sockets.lanes.send(self.wire, at, key, Kind::Rst);
            return Vec::new();
        }
        end.inbox.extend(bytes);
        let (ack, waker) = (end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, key, ack);
        waker.into_iter().collect()
    }

    /// Takes the FIN to `key`, a live end.
    fn fin(&mut self, at: Monotonic, key: Key) -> Vec<Waker> {
        let end = self.end(key);
        end.peer_closed = true;
        let (ack, waker) = (end.ack(), end.reading.take());
        self.sockets.lanes.send(self.wire, at, key, ack);
        self.reap(key);
        waker.into_iter().collect()
    }

    /// Sends the bytes of `key` that its peer's edge allows, then its FIN when it is
    /// queued and no byte is left.
    fn pump(&mut self, now: Monotonic, key: Key) {
        let mss = mss(self.wire, key);
        let end = (self.sockets.ends.get_mut(&key)).expect("invariant: the end lives");
        while !end.outbox.is_empty() && end.sent < end.edge {
            let n = mss.min(end.outbox.len()).min(end.edge - end.sent);
            let bytes = end.outbox.drain(..n).collect();
            end.sent += n;
            self.sockets
                .lanes
                .send(self.wire, now, key, Kind::Data(bytes));
        }
        if end.outbox.is_empty() && end.fin == Fin::Queued {
            end.fin = Fin::Sent;
            self.sockets.lanes.send(self.wire, now, key, Kind::Fin);
        }
    }

    /// Removes `key` when it is an orphan with nothing left to do.
    fn reap(&mut self, key: Key) {
        if self.end(key).finished() {
            self.sockets.ends.remove(&key);
        }
    }

    fn end(&mut self, key: Key) -> &mut End {
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
            lanes.send(&mut wire, Monotonic(0), Key { local, peer }, Kind::Fin);
        }
        lanes.arrive(local, peer);
        assert_eq!(lanes.floors.len(), 1);
        lanes.arrive(local, peer);
        assert!(lanes.floors.is_empty());
    }
}
