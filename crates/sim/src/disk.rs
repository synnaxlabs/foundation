//! The disks of a run, one per node, and the file calls in flight on them.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash};
use std::mem;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::task::{Poll, Waker};

use block::{Block, Unique};
use env::files::{Error, Mode, Operation};
use env::rng::Rng;
use types::time::Monotonic;

/// The bytes of a sector: a write keeps or loses each sector whole.
const SECTOR: u64 = 512;
/// The bytes that a directory takes, as on ext4.
const DIR_BYTES: u64 = 4_096;
/// The longest that a call takes, in nanoseconds.
const DELAY_MAX: u64 = 100_000;
/// The key of the data directory on each disk.
const ROOT: u64 = 0;
/// The Linux code for an I/O error (`EIO`).
const IO: i32 = 5;
/// The Linux code for a path that is there (`EEXIST`).
const EXISTS: i32 = 17;
/// The Linux code for a path through a file (`ENOTDIR`).
const NOT_DIRECTORY: i32 = 20;
/// The Linux code for a file call on a directory (`EISDIR`).
const DIRECTORY: i32 = 21;

/// One file call, as a driver starts it.
pub(crate) enum Call {
    Open(Mode),
    List,
    CreateDir,
    Remove,
    SyncDir,
    Free,
    /// Writes `bytes`, the bytes of `parts`, at `offset`. It keeps `parts` until it
    /// ends.
    Write {
        inode: u64,
        offset: u64,
        bytes: Vec<u8>,
        parts: Vec<Block>,
    },
    /// Reads `len` bytes at `offset`. It keeps `into` until it ends.
    Read {
        inode: u64,
        offset: u64,
        len: u64,
        into: Unique,
    },
    Sync {
        inode: u64,
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
    fn inode(&self) -> Option<u64> {
        match self {
            Self::Write { inode, .. }
            | Self::Read { inode, .. }
            | Self::Sync { inode } => Some(*inode),
            _ => None,
        }
    }
}

/// What a call gives when it succeeds.
pub(crate) enum Done {
    /// A file that the call opened. The call holds it for the descriptor.
    Open {
        inode: u64,
        len: u64,
    },
    Names(Vec<PathBuf>),
    Unit,
    Free(u64),
    Read(Vec<u8>),
}

/// The blocks that a call kept until it ended. Drop them after the lock is released.
pub(crate) enum Held {
    #[expect(
        dead_code,
        reason = "a write holds its parts until it ends, as the `Descriptor` contract asks"
    )]
    Parts(Vec<Block>),
    Into(Unique),
}

/// A call that ended.
pub(crate) struct Ended {
    pub(crate) result: Result<Done, Error>,
    pub(crate) held: Option<Held>,
}

/// Why a call failed, before its path and operation are known.
enum Fail {
    NotFound,
    Full,
    Code(i32),
}

struct Flight {
    node: usize,
    /// The path of the call, or of the file for a call on an open file.
    path: PathBuf,
    call: Call,
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
pub(crate) struct Disks {
    /// The disk of each node.
    nodes: Vec<Disk>,
    flights: BTreeMap<u64, Flight>,
    /// The calls in flight by end time, then key.
    queue: BTreeSet<(Monotonic, u64)>,
    /// The calls that ended, until their futures take the result.
    done: BTreeMap<u64, Ended>,
    rng: Rng,
    /// The last tick. A call's key is the tick of its start, and a file or directory
    /// that it makes takes the same key.
    tick: u64,
}

impl Disks {
    pub(crate) fn new(rng: Rng) -> Self {
        Self {
            nodes: Vec::new(),
            flights: BTreeMap::new(),
            queue: BTreeSet::new(),
            done: BTreeMap::new(),
            rng,
            tick: ROOT,
        }
    }

    /// Adds the disk of a new node, with `bytes` bytes and an empty data directory.
    pub(crate) fn add(&mut self, bytes: u64) {
        let inodes = BTreeMap::from([(ROOT, Inode::Dir(BTreeMap::new()))]);
        self.nodes.push(Disk {
            bytes,
            used: 0,
            inodes,
            faults: Vec::new(),
        });
    }

    /// Makes the next call of `operation` on `path` on `node` fail with code 5.
    pub(crate) fn fail(&mut self, node: usize, path: &Path, operation: Operation) {
        self.nodes[node].faults.push((normal(path), operation));
    }

    fn tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Starts `call` of `node` on `path` at true time `now`, and returns its key.
    pub(crate) fn submit(
        &mut self,
        now: Monotonic,
        node: usize,
        path: &Path,
        call: Call,
    ) -> u64 {
        let key = self.tick();
        let delay = self.rng.below(DELAY_MAX + 1);
        let disk = &mut self.nodes[node];
        let fault = (normal(path), call.operation());
        let fault = disk.faults.iter().position(|aimed| *aimed == fault);
        let failed = fault.map(|at| disk.faults.remove(at)).is_some();
        let mut before = Vec::new();
        if let Some(inode) = call.inode() {
            let data = disk.file(inode);
            data.holds += 1;
            if let Call::Read { offset, len, .. } = call {
                before = data.bytes(offset..offset + len);
            }
        }
        let at = Monotonic(now.0.saturating_add(delay));
        self.queue.insert((at, key));
        let flight = Flight {
            node,
            path: path.to_path_buf(),
            call,
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

    /// Ends the calls due by true time `at`, in order, and hashes each end into
    /// `digest`. Returns the wakers of their futures, and the blocks of the calls
    /// whose futures dropped.
    pub(crate) fn end(
        &mut self,
        at: Monotonic,
        digest: &mut DefaultHasher,
    ) -> (Vec<Waker>, Vec<Held>) {
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
            let ended = self.apply(key, flight);
            (due, key, kind, ended.result.is_ok()).hash(digest);
            if dropped {
                if let Ok(Done::Open { inode, .. }) = ended.result {
                    self.nodes[node].release(inode);
                }
                orphans.extend(ended.held);
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
            failed,
            before,
            dropped,
            ..
        } = flight;
        let segments = segments(&path);
        let disk = &mut self.nodes[node];
        let result = match &call {
            _ if failed => Err(Fail::Code(IO)),
            Call::Open(mode) => disk.open(key, &segments, *mode),
            Call::List => disk.list(&segments),
            Call::CreateDir => disk.create_dir(key, &segments),
            Call::Remove => disk.remove(&segments),
            Call::SyncDir => disk.dir(&segments).map(|_| Done::Unit),
            Call::Free => Ok(Done::Free(disk.bytes - disk.used)),
            Call::Write {
                inode,
                offset,
                bytes,
                ..
            } => {
                let data = disk.file(*inode);
                data.write(*offset, bytes, key, tick, dropped, &mut self.rng);
                Ok(Done::Unit)
            }
            Call::Read {
                inode, offset, len, ..
            } => {
                let writes = writes(&self.flights, *inode);
                let range = *offset..offset + len;
                let data = disk.file(*inode);
                Ok(Done::Read(data.read(
                    range,
                    &before,
                    &writes,
                    &mut self.rng,
                )))
            }
            Call::Sync { .. } => Ok(Done::Unit),
        };
        let operation = call.operation();
        if let Some(inode) = call.inode() {
            disk.release(inode);
        }
        let held = match call {
            Call::Write { parts, .. } => Some(Held::Parts(parts)),
            Call::Read { into, .. } => Some(Held::Into(into)),
            _ => None,
        };
        let result = result.map_err(|fail| match fail {
            Fail::NotFound => Error::NotFound { path },
            Fail::Full => Error::Full { path },
            Fail::Code(code) => Error::Io {
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
        if let Ok(Done::Open { inode, .. }) = ended.result {
            self.nodes[node].release(inode);
        }
        (None, ended.held)
    }

    /// Closes a descriptor of file `inode` of `node`.
    pub(crate) fn close(&mut self, node: usize, inode: u64) {
        self.nodes[node].release(inode);
    }
}

/// The offset and bytes of each write in flight on file `inode` that no fault fails.
fn writes(flights: &BTreeMap<u64, Flight>, inode: u64) -> Vec<(u64, &[u8])> {
    (flights.values())
        .filter(|flight| !flight.failed)
        .filter_map(|flight| match &flight.call {
            Call::Write {
                inode: on,
                offset,
                bytes,
                ..
            } if *on == inode => Some((*offset, bytes.as_slice())),
            _ => None,
        })
        .collect()
}

struct Disk {
    bytes: u64,
    /// The bytes of the directories, and of the files that are linked or held.
    used: u64,
    inodes: BTreeMap<u64, Inode>,
    /// Each fault fails the next call of its operation on its path.
    faults: Vec<(PathBuf, Operation)>,
}

enum Inode {
    File(Data),
    Dir(BTreeMap<OsString, u64>),
}

struct Data {
    len: u64,
    /// The sectors written, by index. A missing sector holds zeros.
    sectors: BTreeMap<u64, Sector>,
    /// The descriptors and the calls in flight that use the file.
    holds: u64,
    /// The file is in a directory.
    linked: bool,
}

struct Sector {
    bytes: [u8; 512],
    /// The tick at which the last write on it ended.
    written: u64,
}

impl Disk {
    /// The directory at `segments`.
    fn dir(&self, segments: &[&OsStr]) -> Result<u64, Fail> {
        let mut at = ROOT;
        for segment in segments {
            let Inode::Dir(entries) = &self.inodes[&at] else {
                return Err(Fail::Code(NOT_DIRECTORY));
            };
            at = *entries.get(*segment).ok_or(Fail::NotFound)?;
        }
        match self.inodes[&at] {
            Inode::Dir(_) => Ok(at),
            Inode::File(_) => Err(Fail::Code(NOT_DIRECTORY)),
        }
    }

    fn entries(&mut self, dir: u64) -> &mut BTreeMap<OsString, u64> {
        match self.inodes.get_mut(&dir) {
            Some(Inode::Dir(entries)) => entries,
            _ => unreachable!("invariant: inode {dir} is a directory"),
        }
    }

    fn file(&mut self, inode: u64) -> &mut Data {
        match self.inodes.get_mut(&inode) {
            Some(Inode::File(data)) => data,
            _ => unreachable!("invariant: open file {inode} is there"),
        }
    }

    /// Takes `bytes` of the free bytes.
    fn take(&mut self, bytes: u64) -> Result<(), Fail> {
        let used = self.used.checked_add(bytes);
        self.used = used.filter(|&used| used <= self.bytes).ok_or(Fail::Full)?;
        Ok(())
    }

    /// Opens the file at `segments`. A file that it makes takes the key `key`.
    fn open(
        &mut self,
        key: u64,
        segments: &[&OsStr],
        mode: Mode,
    ) -> Result<Done, Fail> {
        let Some((name, parent)) = segments.split_last() else {
            return Err(Fail::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = match (self.entries(dir).get(*name).copied(), mode) {
            (Some(inode), _) => inode,
            (None, Mode::Create { len }) => {
                self.take(len)?;
                self.entries(dir).insert(name.into(), key);
                let data = Data {
                    len,
                    sectors: BTreeMap::new(),
                    holds: 0,
                    linked: true,
                };
                self.inodes.insert(key, Inode::File(data));
                key
            }
            (None, Mode::Read | Mode::Write) => return Err(Fail::NotFound),
        };
        let Some(Inode::File(data)) = self.inodes.get_mut(&inode) else {
            return Err(Fail::Code(DIRECTORY));
        };
        data.holds += 1;
        let len = data.len;
        Ok(Done::Open { inode, len })
    }

    fn list(&mut self, segments: &[&OsStr]) -> Result<Done, Fail> {
        let dir = self.dir(segments)?;
        let names = self.entries(dir).keys().map(PathBuf::from);
        Ok(Done::Names(names.collect()))
    }

    /// Makes the directory at `segments`, with the key `key`.
    fn create_dir(&mut self, key: u64, segments: &[&OsStr]) -> Result<Done, Fail> {
        let Some((name, parent)) = segments.split_last() else {
            return Ok(Done::Unit);
        };
        let dir = self.dir(parent)?;
        let found = self.entries(dir).get(*name).copied();
        match found.map(|inode| &self.inodes[&inode]) {
            Some(Inode::Dir(_)) => Ok(Done::Unit),
            Some(Inode::File(_)) => Err(Fail::Code(EXISTS)),
            None => {
                self.take(DIR_BYTES)?;
                self.entries(dir).insert(name.into(), key);
                self.inodes.insert(key, Inode::Dir(BTreeMap::new()));
                Ok(Done::Unit)
            }
        }
    }

    /// Unlinks the file at `segments`. It stays while a hold remains.
    fn remove(&mut self, segments: &[&OsStr]) -> Result<Done, Fail> {
        let Some((name, parent)) = segments.split_last() else {
            return Err(Fail::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = *self.entries(dir).get(*name).ok_or(Fail::NotFound)?;
        let Some(Inode::File(data)) = self.inodes.get_mut(&inode) else {
            return Err(Fail::Code(DIRECTORY));
        };
        data.linked = false;
        self.entries(dir).remove(*name);
        self.collect(inode);
        Ok(Done::Unit)
    }

    /// Drops one hold of file `inode`.
    fn release(&mut self, inode: u64) {
        self.file(inode).holds -= 1;
        self.collect(inode);
    }

    /// Frees file `inode` when it is unlinked and has no hold.
    fn collect(&mut self, inode: u64) {
        let data = self.file(inode);
        if !data.linked && data.holds == 0 {
            self.used -= data.len;
            self.inodes.remove(&inode);
        }
    }
}

impl Data {
    /// The bytes in `range`.
    fn bytes(&self, range: Range<u64>) -> Vec<u8> {
        let mut bytes = vec![0; index(range.end - range.start)];
        for (sector, part) in sectors(range.clone()) {
            if let Some(found) = self.sectors.get(&sector) {
                let to = within(range.start, &part);
                bytes[to].copy_from_slice(&found.bytes[within(sector * SECTOR, &part)]);
            }
        }
        bytes
    }

    /// Ends a write of `bytes` at `offset` that started at tick `started`. Each
    /// sector that another write ended on since then, and each sector of a write
    /// whose future dropped, keeps its bytes or takes the new ones by a coin.
    fn write(
        &mut self,
        offset: u64,
        bytes: &[u8],
        started: u64,
        tick: u64,
        dropped: bool,
        rng: &mut Rng,
    ) {
        for (sector, part) in sectors(offset..offset + len(bytes)) {
            let found = self.sectors.get(&sector);
            let raced = found.is_some_and(|found| found.written > started);
            if (raced || dropped) && rng.below(2) == 0 {
                continue;
            }
            let found = self.sectors.entry(sector).or_insert(Sector {
                bytes: [0; 512],
                written: tick,
            });
            let to = within(sector * SECTOR, &part);
            found.bytes[to].copy_from_slice(&bytes[within(offset, &part)]);
            found.written = tick;
        }
    }

    /// Ends a read of `range`, whose bytes were `before` when it started, while
    /// `writes` (offset and bytes) are in flight on the file. Each sector of the
    /// range takes its bytes at the start, its bytes now, or the bytes of one of
    /// those writes over it, by the run's disk stream.
    fn read(
        &self,
        range: Range<u64>,
        before: &[u8],
        writes: &[(u64, &[u8])],
        rng: &mut Rng,
    ) -> Vec<u8> {
        let mut bytes = self.bytes(range.clone());
        for (_, part) in sectors(range.clone()) {
            let over: Vec<(u64, &[u8])> = (writes.iter().copied())
                .filter(|&(offset, write)| {
                    offset < part.end && part.start < offset + len(write)
                })
                .collect();
            let choices =
                u64::try_from(over.len() + 2).expect("invariant: usize fits u64");
            let to = within(range.start, &part);
            match rng.below(choices) {
                0 => bytes[to.clone()].copy_from_slice(&before[to]),
                1 => {}
                pick => {
                    let (offset, write) = over[index(pick - 2)];
                    let part =
                        part.start.max(offset)..part.end.min(offset + len(write));
                    let to = within(range.start, &part);
                    bytes[to].copy_from_slice(&write[within(offset, &part)]);
                }
            }
        }
        bytes
    }
}

/// Each sector that `range` touches, with the part of `range` in it.
fn sectors(range: Range<u64>) -> impl Iterator<Item = (u64, Range<u64>)> {
    (range.start / SECTOR..range.end.div_ceil(SECTOR)).map(move |sector| {
        let start = (sector * SECTOR).max(range.start);
        let end = ((sector + 1) * SECTOR).min(range.end);
        (sector, start..end)
    })
}

/// `part` as indexes into a buffer that starts at `start`.
fn within(start: u64, part: &Range<u64>) -> Range<usize> {
    index(part.start - start)..index(part.end - start)
}

/// The segments of a checked path: only its names, since `.` adds nothing.
fn segments(path: &Path) -> Vec<&OsStr> {
    (path.components())
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// The path of the same file as `path`, with only its names.
fn normal(path: &Path) -> PathBuf {
    segments(path).into_iter().collect()
}

fn index(at: u64) -> usize {
    usize::try_from(at).expect("invariant: a position in memory fits usize")
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("invariant: usize fits u64")
}
