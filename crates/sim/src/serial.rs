//! The serial lines of a run: their ends, open ports, and bytes in flight.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{DefaultHasher, Hash};
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

/// One end of a line: its node and the path of its port.
pub(crate) type End = (usize, PathBuf);

/// An open end: its line and its side, 0 or 1.
pub(crate) type Side = (usize, usize);

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
    /// The start of the run of characters sent back to back, and their count.
    train: (Monotonic, u64),
    reader: Option<Waker>,
    writer: Option<Waker>,
}

impl Open {
    /// The true time at which the last character of the train arrives, or `None`
    /// past the end of true time.
    fn last(&self) -> Option<Monotonic> {
        let (start, count) = self.train;
        start.checked_add(self.rate.span(count))
    }

    /// Adds a character to the train, and gives the true time it arrives.
    fn send(&mut self) -> Option<Monotonic> {
        self.train.1 += 1;
        let at = self.last()?;
        if self.train.1 == self.rate.num() {
            // `num` characters take exactly `den` seconds, so the start moves with no
            // rounding, and the count stays under `num`.
            self.train = (at, 0);
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
    line: usize,
    /// The side that sent it.
    from: usize,
    settings: Settings,
    /// Its value, after a flip.
    value: u8,
    fault: Option<Fault>,
}

/// The serial lines of a run.
#[derive(Default)]
pub(crate) struct Serial {
    /// The line and side of each end.
    ends: BTreeMap<End, Side>,
    lines: Vec<Line>,
    /// Bytes by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Byte>,
    next: u64,
}

impl Serial {
    /// Sets the line between `a` and `b`, or gives the end that is on another
    /// line. `a` and `b` differ.
    pub(crate) fn join(
        &mut self,
        a: End,
        b: End,
        config: line::Config,
        rng: Rng,
    ) -> Result<(), End> {
        let line = match (self.ends.get(&a), self.ends.get(&b)) {
            (None, None) => {
                let line = self.lines.len();
                self.lines.push(Line {
                    config,
                    rng,
                    ends: [None, None],
                });
                self.ends.insert(a, (line, 0));
                self.ends.insert(b, (line, 1));
                return Ok(());
            }
            (Some(&(one, _)), Some(&(other, _))) if one == other => one,
            (Some(_), _) => return Err(a),
            (None, Some(_)) => return Err(b),
        };
        (self.lines[line].config, self.lines[line].rng) = (config, rng);
        Ok(())
    }

    /// Opens the end at `config.path` of `node`.
    pub(crate) fn open(&mut self, node: usize, config: &Config) -> Result<Side, Error> {
        let end = (node, config.path.clone());
        let Some(&(line, side)) = self.ends.get(&end) else {
            return Err(Error::NotFound { path: end.1 });
        };
        let slot = &mut self.lines[line].ends[side];
        if slot.is_some() {
            return Err(Error::Busy { path: end.1 });
        }
        *slot = Some(Open {
            settings: config.settings,
            rate: config.settings.rate(),
            queue: VecDeque::new(),
            unsent: 0,
            train: (Monotonic::default(), 0),
            reader: None,
            writer: None,
        });
        Ok((line, side))
    }

    /// Closes `side` and loses its bytes in flight. Returns its wakers for the
    /// caller to drop after it releases the lock.
    pub(crate) fn close(&mut self, (line, side): Side) -> [Option<Waker>; 2] {
        self.flights
            .retain(|_, byte| (byte.line, byte.from) != (line, side));
        let open = self.lines[line].ends[side].take().expect(OPEN);
        [open.reader, open.writer]
    }

    /// Reads into `buffer` from the queue of `side`, or keeps `waker` when it is
    /// empty. Returns a waker for the caller to drop after it releases the lock.
    pub(crate) fn read(
        &mut self,
        (line, side): Side,
        waker: Waker,
        buffer: &mut [u8],
    ) -> (Poll<usize>, Option<Waker>) {
        let open = self.lines[line].ends[side].as_mut().expect(OPEN);
        if open.queue.is_empty() {
            return (Poll::Pending, open.reader.replace(waker));
        }
        let count = buffer.len().min(open.queue.len());
        for (slot, byte) in buffer.iter_mut().zip(open.queue.drain(..count)) {
            *slot = byte;
        }
        (Poll::Ready(count), Some(waker))
    }

    /// Sends from `bytes` on `side` at true time `now` while its queue has room, or
    /// keeps `waker` when it has none. Returns a waker for the caller to drop after
    /// it releases the lock.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        (line, side): Side,
        waker: Waker,
        bytes: &[u8],
    ) -> (Poll<usize>, Option<Waker>) {
        let Line { config, rng, ends } = &mut self.lines[line];
        let open = ends[side].as_mut().expect(OPEN);
        let count = bytes.len().min(QUEUE - open.unsent);
        if count == 0 {
            return (Poll::Pending, open.writer.replace(waker));
        }
        if open.last().is_some_and(|last| last <= now) {
            open.train = (now, 0);
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
            // A byte past the end of true time never arrives, and keeps its room.
            let Some(at) = open.send() else { continue };
            self.next += 1;
            let byte = Byte {
                line,
                from: side,
                settings: open.settings,
                value,
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
    pub(crate) fn deliver(
        &mut self,
        at: Monotonic,
        digest: &mut DefaultHasher,
    ) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(flight) = self.flights.first_entry() {
            if flight.key().0 > at {
                break;
            }
            let byte = flight.remove();
            let line = &mut self.lines[byte.line];
            let sender = line.ends[byte.from].as_mut();
            let sender =
                sender.expect("invariant: a close removes the bytes of its end");
            sender.unsent -= 1;
            wakers.extend(sender.writer.take());
            let to = 1 - byte.from;
            let fate = match (&mut line.ends[to], byte.fault) {
                (None, _) => Fate::Dropped,
                (Some(_), Some(Fault::Lost)) => Fate::Lost,
                (Some(open), _) if open.settings != byte.settings => {
                    let value = line.rng.next_u64().to_le_bytes()[0];
                    push(open, value, &mut wakers)
                }
                (Some(open), Some(Fault::Flipped))
                    if open.settings.parity.is_some() =>
                {
                    Fate::Lost
                }
                (Some(open), _) => push(open, byte.value, &mut wakers),
            };
            (at, byte.line, to, fate).hash(digest);
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
