//! The file calls of a run: the disk of each node and the calls in flight on them.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem;
use std::path::{Path, PathBuf};
use std::task::{Poll, Waker};

use block::{Block, Unique};
use env::files::{Error, Mode, Operation};
use env::rng::Rng;
use types::time::Monotonic;

use crate::disk::{self, Cause, Disk, Handle};
use crate::{Crash, EIO};

/// The count of call delays in nanoseconds: a call takes 0 to 100 us.
const DELAYS: u64 = 100_001;

/// One file call, as a driver starts it.
pub(crate) enum Call {
    Open(Mode),
    List,
    CreateDir,
    Remove,
    SyncDir,
    Free,
    Write {
        handle: Handle,
        offset: u64,
        bytes: Vec<u8>,
    },
    Read {
        handle: Handle,
        offset: u64,
        len: u64,
    },
    Sync {
        handle: Handle,
    },
}

impl Call {
    fn operation(&self) -> Operation {
        match self {
            Self::Open(_) => Operation::Open,
            Self::List => Operation::List,
            Self::CreateDir => Operation::CreateDir,
            Self::Remove => Operation::Remove,
            Self::SyncDir => Operation::SyncDir,
            Self::Free => Operation::Free,
            Self::Write { .. } => Operation::WriteAt,
            Self::Read { .. } => Operation::ReadAt,
            Self::Sync { .. } => Operation::Sync,
        }
    }

    /// The open file that it acts on, if any.
    fn handle(&self) -> Option<Handle> {
        match self {
            Self::Write { handle, .. }
            | Self::Read { handle, .. }
            | Self::Sync { handle } => Some(*handle),
            _ => None,
        }
    }
}

/// What a call gives when it succeeds.
pub(crate) enum Done {
    /// A file that the call opened. The call holds it for the descriptor.
    Open {
        handle: Handle,
        len: u64,
    },
    Names(Vec<PathBuf>),
    Unit,
    Free(u64),
    Read(Vec<u8>),
}

/// The blocks that a call keeps until it ends. Drop them after the lock is released.
pub(crate) enum Held {
    #[expect(dead_code, reason = "a write keeps its parts until it ends")]
    Parts(Vec<Block>),
    Into(Unique),
}

/// A call that ended.
pub(crate) struct Ended {
    pub(crate) result: Result<Done, Error>,
    pub(crate) held: Option<Held>,
}

struct Flight {
    node: usize,
    /// The path of the call, or of the file for a call on an open file.
    path: PathBuf,
    call: Call,
    held: Option<Held>,
    /// A fault fails the call when it ends.
    failed: bool,
    /// A read's bytes when it started. Empty for other calls.
    before: Vec<u8>,
    waker: Option<Waker>,
    /// Its future dropped, so nothing takes its result.
    dropped: bool,
}

/// The disks of a run and the calls in flight. A call takes effect when it ends,
/// after a delay of up to 100 us from the run's disk stream.
pub(crate) struct Files {
    /// The disk of each node.
    disks: Vec<Disk>,
    /// Each fault fails the next call of its operation on its path on its node.
    faults: Vec<(usize, PathBuf, Operation)>,
    flights: BTreeMap<u64, Flight>,
    /// The calls in flight by end time, then key.
    queue: BTreeSet<(Monotonic, u64)>,
    /// The calls that ended, until their futures take the result.
    done: BTreeMap<u64, Ended>,
    /// The waker of each close that waits for the calls of its descriptor, by the key
    /// of its handle.
    closes: BTreeMap<u64, Waker>,
    rng: Rng,
    /// The last tick. A call's key is the tick of its start, a file or directory
    /// that it makes takes the same key, and a write takes a tick when it ends. One
    /// counter orders them all, so a sector written at a tick past a call's key was
    /// written after the call started.
    tick: u64,
    /// A hash of every end of a call, in order.
    digest: DefaultHasher,
}

impl Files {
    pub(crate) fn new(rng: Rng) -> Self {
        Self {
            disks: Vec::new(),
            faults: Vec::new(),
            flights: BTreeMap::new(),
            queue: BTreeSet::new(),
            done: BTreeMap::new(),
            closes: BTreeMap::new(),
            rng,
            tick: disk::ROOT,
            digest: DefaultHasher::new(),
        }
    }

    /// Adds the disk of a new node, with `bytes` bytes and an empty data directory.
    pub(crate) fn add(&mut self, bytes: u64) {
        self.disks.push(Disk::new(bytes));
    }

    /// Makes the next call of `operation` on `path` on `node` fail with code 5.
    pub(crate) fn fail(&mut self, node: usize, path: &Path, operation: Operation) {
        self.faults.push((node, disk::normal(path), operation));
    }

    fn tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Starts `call` of `node` on `path` at true time `now`, keeping `held` until it
    /// ends, and returns its key.
    pub(crate) fn submit(
        &mut self,
        now: Monotonic,
        node: usize,
        path: &Path,
        call: Call,
        held: Option<Held>,
    ) -> u64 {
        let key = self.tick();
        let delay = self.rng.below(DELAYS);
        let fault = (node, disk::normal(path), call.operation());
        let fault = self.faults.iter().position(|aimed| *aimed == fault);
        let failed = fault.map(|at| self.faults.remove(at)).is_some();
        let disk = &mut self.disks[node];
        let mut before = Vec::new();
        if let Some(handle) = call.handle() {
            disk.hold(handle);
            if let Call::Read { offset, len, .. } = call {
                before = disk.file(handle.inode).bytes(offset..offset + len);
            }
        }
        let at = Monotonic(now.0.saturating_add(delay));
        self.queue.insert((at, key));
        let flight = Flight {
            node,
            path: path.to_path_buf(),
            call,
            held,
            failed,
            before,
            waker: None,
            dropped: false,
        };
        self.flights.insert(key, flight);
        key
    }

    /// The true time at which the first call in flight ends.
    pub(crate) fn first(&self) -> Option<Monotonic> {
        self.queue.first().map(|&(at, _)| at)
    }

    /// A hash of every end of a call so far: its time, key, kind, and success.
    pub(crate) fn digest(&self) -> u64 {
        self.digest.finish()
    }

    /// Ends the calls due by true time `at`, in order. Returns the wakers of their
    /// futures, and the blocks of the calls whose futures dropped.
    pub(crate) fn end(&mut self, at: Monotonic) -> (Vec<Waker>, Vec<Held>) {
        let (mut wakers, mut orphans) = (Vec::new(), Vec::new());
        while let Some(&(due, key)) = self.queue.first() {
            if due > at {
                break;
            }
            self.queue.pop_first();
            let mut flight = (self.flights.remove(&key))
                .expect("invariant: a queued call is in flight");
            let (node, dropped, waker) =
                (flight.node, flight.dropped, flight.waker.take());
            let kind = mem::discriminant(&flight.call);
            let close = flight.call.handle().map(|handle| handle.key);
            wakers.extend(close.and_then(|key| self.closes.remove(&key)));
            let ended = self.apply(key, flight);
            (due, key, kind, ended.result.is_ok()).hash(&mut self.digest);
            if dropped {
                orphans.extend(self.discard(node, ended));
            } else {
                self.done.insert(key, ended);
                wakers.extend(waker);
            }
        }
        (wakers, orphans)
    }

    /// Applies call `key`, which ends now.
    fn apply(&mut self, key: u64, flight: Flight) -> Ended {
        let tick = self.tick();
        let Flight {
            node,
            path,
            call,
            held,
            failed,
            before,
            dropped,
            ..
        } = flight;
        let disk = &mut self.disks[node];
        let result = match &call {
            Call::Sync { handle } if failed => {
                disk.file(handle.inode).tear(key, &mut self.rng);
                Err(Cause::Code(EIO))
            }
            _ if failed => Err(Cause::Code(EIO)),
            Call::Open(mode) => disk
                .open(key, &path, *mode)
                .map(|(handle, len)| Done::Open { handle, len }),
            Call::List => disk.list(&path).map(Done::Names),
            Call::CreateDir => disk.create_dir(key, &path).map(|()| Done::Unit),
            Call::Remove => disk.remove(&path).map(|()| Done::Unit),
            Call::SyncDir => disk.sync_dir(&path).map(|()| Done::Unit),
            Call::Free => Ok(Done::Free(disk.free())),
            Call::Write {
                handle,
                offset,
                bytes,
            } => {
                let file = disk.file(handle.inode);
                file.write(*offset, bytes, key, tick, dropped, &mut self.rng);
                Ok(Done::Unit)
            }
            Call::Read {
                handle,
                offset,
                len,
            } => {
                let writes = writes(&self.flights, handle.inode);
                let file = disk.file(handle.inode);
                let range = *offset..offset + len;
                let bytes = file.read(range, &before, &writes, &mut self.rng);
                Ok(Done::Read(bytes))
            }
            Call::Sync { handle } => {
                disk.file(handle.inode).sync(key);
                Ok(Done::Unit)
            }
        };
        if let Some(handle) = call.handle() {
            disk.release(handle);
        }
        let operation = call.operation();
        let result = result.map_err(|cause| match cause {
            Cause::NotFound => Error::NotFound { path },
            Cause::Full => Error::Full { path },
            Cause::Busy => Error::Busy { path },
            Cause::Code(code) => Error::Io {
                path,
                operation,
                code,
            },
        });
        Ended { result, held }
    }

    /// Takes the result of call `key` when it has ended, or keeps `waker` to wake
    /// when it ends. Returns the waker to drop after the lock is released.
    pub(crate) fn poll(
        &mut self,
        key: u64,
        waker: Waker,
    ) -> (Poll<Ended>, Option<Waker>) {
        if let Some(ended) = self.done.remove(&key) {
            return (Poll::Ready(ended), Some(waker));
        }
        let flight = (self.flights.get_mut(&key))
            .expect("invariant: a call that has not ended is in flight");
        (Poll::Pending, flight.waker.replace(waker))
    }

    /// The future of call `key` of `node` dropped before it took the result. A call
    /// in flight still ends. Returns what to drop after the lock is released.
    pub(crate) fn abandon(
        &mut self,
        node: usize,
        key: u64,
    ) -> (Option<Waker>, Option<Held>) {
        if let Some(flight) = self.flights.get_mut(&key) {
            flight.dropped = true;
            return (flight.waker.take(), None);
        }
        let ended = (self.done.remove(&key))
            .expect("invariant: a call whose result was not taken has ended");
        (None, self.discard(node, ended))
    }

    /// Drops call `ended` of `node`, whose future dropped: an open releases the file
    /// it opened. Returns the blocks of the call, for the caller to drop after it
    /// releases the lock.
    fn discard(&mut self, node: usize, ended: Ended) -> Option<Held> {
        if let Ok(Done::Open { handle, .. }) = ended.result {
            self.disks[node].release(handle);
        }
        ended.held
    }

    /// Crashes `node` by `crash` at true time `at`. Each call in flight of the node
    /// ends now as one whose future dropped, a leaked one too, in the order of its
    /// end time: after a `Process` crash each takes effect, and after a `Power` crash
    /// only each write does, and the disk keeps what is durable. Then each file of
    /// the node loses its holds, those of leaked descriptors too. Returns the blocks
    /// of the calls, for the caller to drop after it releases the lock.
    pub(crate) fn crash(
        &mut self,
        node: usize,
        at: Monotonic,
        crash: Crash,
    ) -> Vec<Held> {
        let flights = &self.flights;
        let (cut, queue): (BTreeSet<_>, _) = mem::take(&mut self.queue)
            .into_iter()
            .partition(|(_, key)| flights[key].node == node);
        self.queue = queue;
        let mut orphans = Vec::new();
        for (_, key) in cut {
            let mut flight = (self.flights.remove(&key))
                .expect("invariant: a queued call is in flight");
            flight.dropped = true;
            let kind = mem::discriminant(&flight.call);
            let applied =
                crash == Crash::Process || matches!(flight.call, Call::Write { .. });
            let (ok, held) = if applied {
                let ended = self.apply(key, flight);
                (ended.result.is_ok(), ended.held)
            } else {
                (false, flight.held)
            };
            (at, key, kind, ok).hash(&mut self.digest);
            orphans.extend(held);
        }
        self.disks[node].crash(crash, &mut self.rng);
        orphans
    }

    /// Polls the close of descriptor `handle`: ready when none of its calls is in
    /// flight. Else it keeps `waker` to wake when one of them ends. Returns the waker
    /// to drop after the lock is released.
    pub(crate) fn poll_close(
        &mut self,
        handle: Handle,
        waker: Waker,
    ) -> (Poll<()>, Option<Waker>) {
        let mut calls = self
            .flights
            .values()
            .filter_map(|flight| flight.call.handle());
        if calls.all(|held| held.key != handle.key) {
            return (Poll::Ready(()), Some(waker));
        }
        (Poll::Pending, self.closes.insert(handle.key, waker))
    }

    /// Closes descriptor `handle` of `node`. Returns the waker of its close, to drop
    /// after the lock is released.
    pub(crate) fn release(&mut self, node: usize, handle: Handle) -> Option<Waker> {
        self.disks[node].release(handle);
        self.closes.remove(&handle.key)
    }
}

/// The offset and bytes of each write in flight on file `inode` that no fault fails.
fn writes(flights: &BTreeMap<u64, Flight>, inode: u64) -> Vec<(u64, &[u8])> {
    (flights.values())
        .filter(|flight| !flight.failed)
        .filter_map(|flight| match &flight.call {
            Call::Write {
                handle,
                offset,
                bytes,
            } if handle.inode == inode => Some((*offset, bytes.as_slice())),
            _ => None,
        })
        .collect()
}
