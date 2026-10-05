//! The serial lines of a run: their ends, open ports, and bytes in flight.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::task::{Poll, Waker};

use env::rng::Rng;
use env::serial::{Config, Error, Settings};
use types::time::{Monotonic, Rate};

use crate::chance::roll;
use crate::line;

/// The bytes that each end holds not yet arrived, and not yet read: the transmit
/// buffer of a Linux UART and the read buffer of a Linux TTY.
const QUEUE: usize = 4_096;

/// The panic message of a broken invariant: a port driver outlives its end.
const OPEN: &str = "invariant: a port is open while its driver lives";

/// One end of a line.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct End {
    line: usize,
    /// 0 or 1.
    side: usize,
}

impl End {
    fn other(self) -> Self {
        Self {
            line: self.line,
            side: 1 - self.side,
        }
    }
}

/// What happens to a byte at its arrival, for the digest.
#[derive(Clone, Copy, Hash)]
enum Fate {
    /// The line lost it, or the receiver found a parity error.
    Lost,
    /// It arrived at an end that is not open, or at a full queue.
    Dropped,
    /// It arrived in the queue of the end.
    Queued,
}

/// An open end of a line.
struct Open {
    settings: Settings,
    /// The rate of `settings`.
    rate: Rate,
    /// The bytes that arrived, not yet read.
    queue: VecDeque<u8>,
    /// The bytes written that have not arrived.
    unsent: usize,
    /// The start of the run of characters sent back to back.
    start: Monotonic,
    /// The characters of the run since `start`.
    sent: u64,
    reader: Option<Waker>,
    writer: Option<Waker>,
}

impl Open {
    /// Adds a character sent at true time `now` to the run, or starts a new run when
    /// the last character arrived by `now`. Gives the true time it arrives, or
    /// `None` past the end of true time.
    fn send(&mut self, now: Monotonic) -> Option<Monotonic> {
        let last = self.start.checked_add(self.rate.span(self.sent));
        if last.is_some_and(|last| last <= now) {
            (self.start, self.sent) = (now, 0);
        }
        self.sent += 1;
        let at = self.start.checked_add(self.rate.span(self.sent))?;
        if self.sent == self.rate.num() {
            // `num` characters take exactly `den` seconds, so the start moves with no
            // rounding, and `sent` stays under `num`.
            (self.start, self.sent) = (at, 0);
        }
        Some(at)
    }
}

struct Line {
    config: line::Config,
    rng: Rng,
    ends: [Option<Open>; 2],
}

/// A fault of the line, drawn as a byte is sent.
#[derive(Clone, Copy)]
enum Fault {
    Lost,
    Flipped,
}

/// A byte in flight.
struct Byte {
    from: End,
    settings: Settings,
    /// Its value, after a flip.
    value: u8,
    /// The value that arrives when the settings of the ends differ.
    noise: u8,
    fault: Option<Fault>,
}

/// The serial lines of a run.
pub(crate) struct Serial {
    /// The end at each port of a node: the node and the path.
    ends: BTreeMap<(usize, PathBuf), End>,
    lines: Vec<Line>,
    /// Bytes by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Byte>,
    next: u64,
    /// The stream that each line draws its own stream from.
    rng: Rng,
    digest: DefaultHasher,
}

impl Serial {
    pub(crate) fn new(rng: Rng) -> Self {
        Self {
            ends: BTreeMap::new(),
            lines: Vec::new(),
            flights: BTreeMap::new(),
            next: 0,
            rng,
            digest: DefaultHasher::new(),
        }
    }

    /// A hash of every byte arrival so far: its time, end, and fate.
    pub(crate) fn digest(&self) -> u64 {
        self.digest.finish()
    }

    /// Sets the line between ports `a` and `b`, with a new stream of faults, or
    /// gives the port that is on another line. `a` and `b` differ.
    pub(crate) fn join(
        &mut self,
        a: (usize, PathBuf),
        b: (usize, PathBuf),
        config: line::Config,
    ) -> Result<(), (usize, PathBuf)> {
        let line = match (self.ends.get(&a), self.ends.get(&b)) {
            (None, None) => {
                let line = self.lines.len();
                self.lines.push(Line {
                    config,
                    rng: Rng::from_seed(self.rng.next_u64()),
                    ends: [None, None],
                });
                self.ends.insert(a, End { line, side: 0 });
                self.ends.insert(b, End { line, side: 1 });
                return Ok(());
            }
            (Some(one), Some(other)) if one.line == other.line => one.line,
            (Some(_), _) => return Err(a),
            (None, Some(_)) => return Err(b),
        };
        let rng = Rng::from_seed(self.rng.next_u64());
        (self.lines[line].config, self.lines[line].rng) = (config, rng);
        Ok(())
    }

    /// Opens the end at `config.path` of `node`.
    pub(crate) fn open(&mut self, node: usize, config: &Config) -> Result<End, Error> {
        let port = (node, config.path.clone());
        let Some(&end) = self.ends.get(&port) else {
            return Err(Error::NotFound { path: port.1 });
        };
        let slot = &mut self.lines[end.line].ends[end.side];
        if slot.is_some() {
            return Err(Error::Busy { path: port.1 });
        }
        *slot = Some(Open {
            settings: config.settings,
            rate: config.settings.rate(),
            queue: VecDeque::new(),
            unsent: 0,
            start: Monotonic::default(),
            sent: 0,
            reader: None,
            writer: None,
        });
        Ok(end)
    }

    /// Closes `end` and loses its bytes in flight. Returns its wakers for the caller
    /// to drop after it releases the lock.
    pub(crate) fn close(&mut self, end: End) -> [Option<Waker>; 2] {
        self.flights.retain(|_, byte| byte.from != end);
        let open = self.lines[end.line].ends[end.side].take().expect(OPEN);
        [open.reader, open.writer]
    }

    /// Reads into `buffer` from the queue of `end`, or keeps `waker` when it is
    /// empty. Returns a waker for the caller to drop after it releases the lock.
    pub(crate) fn read(
        &mut self,
        end: End,
        waker: Waker,
        buffer: &mut [u8],
    ) -> (Poll<usize>, Option<Waker>) {
        let open = self.lines[end.line].ends[end.side].as_mut().expect(OPEN);
        if open.queue.is_empty() {
            return (Poll::Pending, open.reader.replace(waker));
        }
        let count = buffer.len().min(open.queue.len());
        for (slot, byte) in buffer.iter_mut().zip(open.queue.drain(..count)) {
            *slot = byte;
        }
        (Poll::Ready(count), Some(waker))
    }

    /// Sends from `bytes` on `end` at true time `now` while its queue has room, or
    /// keeps `waker` when it has none. Returns a waker for the caller to drop after
    /// it releases the lock.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        end: End,
        waker: Waker,
        bytes: &[u8],
    ) -> (Poll<usize>, Option<Waker>) {
        let Line { config, rng, ends } = &mut self.lines[end.line];
        let open = ends[end.side].as_mut().expect(OPEN);
        let count = bytes.len().min(QUEUE - open.unsent);
        if count == 0 {
            return (Poll::Pending, open.writer.replace(waker));
        }
        for &value in &bytes[..count] {
            open.unsent += 1;
            let (fault, value) = if roll(rng, config.loss) {
                (Some(Fault::Lost), value)
            } else if roll(rng, config.flip) {
                (Some(Fault::Flipped), value ^ (1 << rng.below(8)))
            } else {
                (None, value)
            };
            let noise = rng.next_u64().to_le_bytes()[0];
            // A byte past the end of true time never arrives, and keeps its room.
            let Some(at) = open.send(now) else { continue };
            self.next += 1;
            let byte = Byte {
                from: end,
                settings: open.settings,
                value,
                noise,
                fault,
            };
            self.flights.insert((at, self.next), byte);
        }
        (Poll::Ready(count), Some(waker))
    }

    /// The true time of the first arrival.
    pub(crate) fn first(&self) -> Option<Monotonic> {
        self.flights.first_key_value().map(|(&(at, _), _)| at)
    }

    /// Delivers the bytes that arrive by true time `at`, and returns the wakers of
    /// the ends that read them and of the ends whose queues get room.
    pub(crate) fn deliver(&mut self, at: Monotonic) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(flight) = self.flights.first_entry() {
            if flight.key().0 > at {
                break;
            }
            let byte = flight.remove();
            let ends = &mut self.lines[byte.from.line].ends;
            let sender = ends[byte.from.side].as_mut();
            let sender =
                sender.expect("invariant: a close removes the bytes of its end");
            sender.unsent -= 1;
            wakers.extend(sender.writer.take());
            let to = byte.from.other();
            let fate = match (&mut ends[to.side], byte.fault) {
                (None, _) => Fate::Dropped,
                (Some(_), Some(Fault::Lost)) => Fate::Lost,
                (Some(open), _) if open.settings != byte.settings => {
                    push(open, byte.noise, &mut wakers)
                }
                (Some(open), Some(Fault::Flipped))
                    if open.settings.parity.is_some() =>
                {
                    Fate::Lost
                }
                (Some(open), _) => push(open, byte.value, &mut wakers),
            };
            (at, to, fate).hash(&mut self.digest);
        }
        wakers
    }
}

/// Queues `value` at `open` while its queue has room, and adds its reader to
/// `wakers`.
fn push(open: &mut Open, value: u8, wakers: &mut Vec<Waker>) -> Fate {
    if open.queue.len() == QUEUE {
        return Fate::Dropped;
    }
    open.queue.push_back(value);
    wakers.extend(open.reader.take());
    Fate::Queued
}
