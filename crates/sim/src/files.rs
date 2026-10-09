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
    Rename {
        handle: Handle,
        to: PathBuf,
    },
    /// A remove through a write handle: the entry must name its file.
    Unlink {
        handle: Handle,
    },
}

impl Call {
    fn operation(&self) -> Operation {
        match self {
            Self::Open(_) => Operation::Open,
            Self::List => Operation::List,
            Self::CreateDir => Operation::CreateDir,
            Self::Remove | Self::Unlink { .. } => Operation::Remove,
            Self::SyncDir => Operation::SyncDir,
            Self::Free => Operation::Free,
            Self::Write { .. } => Operation::WriteAt,
            Self::Read { .. } => Operation::ReadAt,
            Self::Sync { .. } => Operation::Sync,
            Self::Rename { .. } => Operation::Rename,
        }
    }

    /// The open file that it acts on, if any.
    fn handle(&self) -> Option<Handle> {
        match self {
            Self::Write { handle, .. }
            | Self::Read { handle, .. }
            | Self::Sync { handle }
            | Self::Rename { handle, .. }
            | Self::Unlink { handle } => Some(*handle),
            _ => None,
        }
    }
}

/// The error of `operation` on `path`, which failed by `cause`.
fn error(cause: Cause, path: PathBuf, operation: Operation) -> Error {
    match cause {
        Cause::NotFound => Error::NotFound { path },
        Cause::Full => Error::Full { path },
        Cause::Busy => Error::Busy { path },
        Cause::Exists(path) => Error::Exists { path },
        Cause::Code(code) => Error::Io {
            path,
            operation,
            code,
        },
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

/// What a crash leaves of a [`Mode::Create`] open in flight that makes a file. The
/// file system can make the entry and allocate in either order.
#[derive(Clone, Copy, Hash)]
enum Cut {
    /// The open took effect.
    Whole,
    /// The entry, with no bytes.
    Empty,
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
    /// The node and result of each call that ended, until its future takes the
    /// result.
    done: BTreeMap<u64, (usize, Ended)>,
    /// The waker of each close that waits for the calls of its descriptor, by the key
    /// of its handle.
    closing: BTreeMap<u64, Waker>,
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
            closing: BTreeMap::new(),
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
        path: PathBuf,
        call: Call,
        held: Option<Held>,
    ) -> u64 {
        let key = self.tick();
        let delay = self.rng.below(DELAYS);
        let fault = (node, disk::normal(&path), call.operation());
        let fault = self.faults.iter().position(|aimed| *aimed == fault);
        let failed = fault.map(|at| self.faults.remove(at)).is_some();
        let disk = &mut self.disks[node];
        let mut before = Vec::new();
        if let Some(handle) = call.handle() {
            disk.hold(handle);
            if let Call::Read { offset, len, .. } = call {
                let range = offset..offset + len;
                before = disk.file(handle.inode).start_read(range, &mut self.rng);
            }
        }
        let at = Monotonic(now.0.saturating_add(delay));
        self.queue.insert((at, key));
        let flight = Flight {
            node,
            path,
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
            wakers.extend(close.and_then(|key| self.closing.remove(&key)));
            let ended = self.apply(key, flight);
            (due, key, kind, ended.result.is_ok()).hash(&mut self.digest);
            if dropped {
                orphans.extend(self.discard(node, ended));
            } else {
                self.done.insert(key, (node, ended));
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
            Call::Rename { handle, to } => {
                disk.rename(*handle, &path, to).map(|()| Done::Unit)
            }
            Call::Unlink { handle } => {
                disk.unlink(handle.inode, &path).map(|()| Done::Unit)
            }
        };
        if let Some(handle) = call.handle() {
            disk.release(handle);
        }
        let result = result.map_err(|cause| error(cause, path, call.operation()));
        Ended { result, held }
    }

    /// Takes the result of call `key` when it has ended, or keeps `waker` to wake
    /// when it ends. Returns the waker to drop after the lock is released.
    pub(crate) fn poll(
        &mut self,
        key: u64,
        waker: Waker,
    ) -> (Poll<Ended>, Option<Waker>) {
        if let Some((_, ended)) = self.done.remove(&key) {
            return (Poll::Ready(ended), Some(waker));
        }
        let flight = (self.flights.get_mut(&key))
            .expect("invariant: a call that has not ended is in flight");
        (Poll::Pending, flight.waker.replace(waker))
    }

    /// The future of call `key` dropped before it took the result. A call in flight
    /// still ends. Returns what to drop after the lock is released.
    pub(crate) fn abandon(&mut self, key: u64) -> (Option<Waker>, Option<Held>) {
        if let Some(flight) = self.flights.get_mut(&key) {
            flight.dropped = true;
            return (flight.waker.take(), None);
        }
        let (node, ended) = (self.done.remove(&key))
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

    /// Crashes `node` by `crash` at true time `at`: each call, result, close, and hold
    /// of the node ends, a leaked one too. A call in flight ends as one whose future
    /// dropped, in the order of its end time. A create open in flight that makes a file
    /// can make it with no bytes. After a `Power` crash a `sync` or `sync_dir` in
    /// flight has no effect, and the disk keeps what is durable and a prefix of its
    /// log. Returns the wakers of the closes and the blocks of the calls, for the
    /// caller to drop after it releases the lock.
    pub(crate) fn crash(
        &mut self,
        node: usize,
        at: Monotonic,
        crash: Crash,
    ) -> (Vec<Waker>, Vec<Held>) {
        let flights = &self.flights;
        let (cut, queue): (BTreeSet<_>, _) = mem::take(&mut self.queue)
            .into_iter()
            .partition(|(_, key)| flights[key].node == node);
        self.queue = queue;
        let power = crash == Crash::Power;
        let (mut closes, mut orphans) = (Vec::new(), Vec::new());
        for (_, key) in cut {
            let mut flight = (self.flights.remove(&key))
                .expect("invariant: a queued call is in flight");
            flight.dropped = true;
            let close = flight.call.handle().map(|handle| handle.key);
            closes.extend(close.and_then(|key| self.closing.remove(&key)));
            let kind = mem::discriminant(&flight.call);
            let drawn = (matches!(flight.call, Call::Open(Mode::Create { .. }))
                && !flight.failed
                && self.disks[node].makes(&flight.path))
            .then(|| match self.rng.below(2) {
                0 => Cut::Whole,
                _ => Cut::Empty,
            });
            if let Some(Cut::Empty) = drawn {
                flight.call = Call::Open(Mode::Create { len: 0 });
            }
            // A power cut loses what a sync in flight would make durable.
            let synced = matches!(flight.call, Call::Sync { .. } | Call::SyncDir);
            let (ok, held) = if power && synced {
                (false, flight.held)
            } else {
                let ended = self.apply(key, flight);
                (ended.result.is_ok(), ended.held)
            };
            (at, key, kind, drawn, ok).hash(&mut self.digest);
            orphans.extend(held);
        }
        let leaked: Vec<_> = (self.done)
            .extract_if(.., |_, (owner, _)| *owner == node)
            .collect();
        for (_, (_, ended)) in leaked {
            orphans.extend(self.discard(node, ended));
        }
        let kept = self.disks[node].crash(crash, &mut self.rng);
        kept.hash(&mut self.digest);
        (closes, orphans)
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
        (Poll::Pending, self.closing.insert(handle.key, waker))
    }

    /// The path of descriptor `handle` of `node` now, as its open or rename gave it.
    pub(crate) fn path(&self, node: usize, handle: Handle) -> PathBuf {
        self.disks[node].path(handle).to_path_buf()
    }

    /// Makes `handle`, which an open of `path` on `node` gave, a descriptor.
    pub(crate) fn opened(&mut self, node: usize, handle: Handle, path: &Path) {
        self.disks[node].opened(handle, path);
    }

    /// Closes descriptor `handle` of `node`. Returns the waker of its close, to drop
    /// after the lock is released.
    pub(crate) fn close(&mut self, node: usize, handle: Handle) -> Option<Waker> {
        self.disks[node].close(handle);
        self.closing.remove(&handle.key)
    }

    /// The path of each descriptor that `node` closed, in order.
    pub(crate) fn closes(&self, node: usize) -> Vec<PathBuf> {
        self.disks[node].closes().to_vec()
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
