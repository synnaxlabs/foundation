//! The file system of one node: directories, and sparse files of sectors.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::mem;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use env::files::Mode;
use env::rng::Rng;

use crate::Crash;

/// The key of the data directory.
pub(crate) const ROOT: u64 = 0;
/// [`env::files::SECTOR`] as a file offset.
const SECTOR: u64 = env::files::SECTOR as u64;
/// The bytes that a directory takes, as on ext4.
const DIR_BYTES: u64 = 4_096;
/// The Linux code for a path that is there (`EEXIST`).
const EXISTS: i32 = 17;
/// The Linux code for a path through a file (`ENOTDIR`).
const NOT_DIRECTORY: i32 = 20;
/// The Linux code for a file call on a directory (`EISDIR`).
const DIRECTORY: i32 = 21;

/// Why a call failed, before its path and operation are known.
pub(crate) enum Cause {
    NotFound,
    Full,
    /// A write descriptor or its calls hold the file.
    Busy,
    /// The new name of a rename is taken.
    Exists(PathBuf),
    Code(i32),
}

/// An open file, and whether its descriptor may write. A write descriptor and its
/// calls in flight hold the file against another write open.
#[derive(Clone, Copy)]
pub(crate) struct Handle {
    pub(crate) inode: u64,
    pub(crate) writable: bool,
    /// The key of the open that made it. No other handle of the run has it. `files`
    /// finds the calls and the close of a descriptor by it.
    pub(crate) key: u64,
}

pub(crate) struct Disk {
    bytes: u64,
    /// The bytes of the directories, and of the files that a directory entry names
    /// or a hold keeps.
    used: u64,
    inodes: BTreeMap<u64, Inode>,
}

/// What an open finds.
enum Target<'a> {
    /// The file `inode` at the path.
    File(u64),
    /// No entry: a create makes `name` in directory `dir` with `len` bytes.
    New { dir: u64, name: &'a OsStr, len: u64 },
}

enum Inode {
    File(File),
    Dir(Dir),
}

#[derive(Default)]
struct Dir {
    entries: BTreeMap<OsString, u64>,
    /// The entries when the last `sync_dir` on the directory ended.
    durable: BTreeMap<OsString, u64>,
}

pub(crate) struct File {
    len: u64,
    /// The sectors written, by index. A missing sector holds zeros.
    sectors: BTreeMap<u64, Sector>,
    /// The sectors whose `writes` is not empty.
    dirty: BTreeSet<u64>,
    /// The descriptors and the calls in flight that use the file.
    holds: u64,
    /// The holds of a write descriptor and of its calls.
    writers: u64,
    /// An entry names the file.
    linked: bool,
    /// A durable entry names the file.
    durable: bool,
}

/// The durable bytes of a sector, its clean bytes in the cache, and its writes.
struct Sector {
    durable: [u8; env::files::SECTOR],
    /// The bytes under the first write, clean in the cache. They differ from
    /// `durable` only after a sync that failed, until the cache drops them.
    clean: [u8; env::files::SECTOR],
    /// The writes over `clean` that no sync covered, in one order that the times of
    /// their calls allow. A write that overlapped another in flight can be in the
    /// order as up to three parts, each at its own place.
    writes: Vec<Write>,
}

/// One write on a sector, or a part of one.
struct Write {
    /// The tick at which it ended.
    written: u64,
    /// The bytes of the sector that it covered.
    covered: Range<usize>,
    /// Its bytes over `covered`.
    bytes: Vec<u8>,
    /// The sector after it and each write before it.
    after: [u8; env::files::SECTOR],
}

impl Disk {
    /// A disk of `bytes` bytes with an empty data directory.
    pub(crate) fn new(bytes: u64) -> Self {
        Self {
            bytes,
            used: 0,
            inodes: BTreeMap::from([(ROOT, Inode::Dir(Dir::default()))]),
        }
    }

    pub(crate) fn free(&self) -> u64 {
        self.bytes - self.used
    }

    /// The directory at `segments`.
    fn dir(&self, segments: &[&OsStr]) -> Result<u64, Cause> {
        let mut at = ROOT;
        for segment in segments {
            let Inode::Dir(dir) = &self.inodes[&at] else {
                return Err(Cause::Code(NOT_DIRECTORY));
            };
            at = *dir.entries.get(*segment).ok_or(Cause::NotFound)?;
        }
        match self.inodes[&at] {
            Inode::Dir(_) => Ok(at),
            Inode::File(_) => Err(Cause::Code(NOT_DIRECTORY)),
        }
    }

    /// Directory `key`.
    fn dir_mut(&mut self, key: u64) -> &mut Dir {
        match self.inodes.get_mut(&key) {
            Some(Inode::Dir(dir)) => dir,
            _ => unreachable!("invariant: inode {key} is a directory"),
        }
    }

    /// Open file `inode`.
    pub(crate) fn file(&mut self, inode: u64) -> &mut File {
        match self.inodes.get_mut(&inode) {
            Some(Inode::File(file)) => file,
            _ => unreachable!("invariant: open file {inode} is there"),
        }
    }

    /// Takes `bytes` of the free bytes.
    fn take(&mut self, bytes: u64) -> Result<(), Cause> {
        let used = self.used.checked_add(bytes);
        self.used = used.filter(|&used| used <= self.bytes).ok_or(Cause::Full)?;
        Ok(())
    }

    /// The file `inode` that an entry names. A `slashed` path names only a directory.
    fn named(&mut self, inode: u64, slashed: bool) -> Result<&mut File, Cause> {
        match self.inodes.get_mut(&inode) {
            Some(Inode::File(_)) if slashed => Err(Cause::Code(NOT_DIRECTORY)),
            Some(Inode::File(file)) => Ok(file),
            _ => Err(Cause::Code(DIRECTORY)),
        }
    }

    /// Opens the file at `path`, and gives its handle and length. The open holds it.
    /// A file that it makes takes the key `key`.
    pub(crate) fn open(
        &mut self,
        key: u64,
        path: &Path,
        mode: Mode,
    ) -> Result<(Handle, u64), Cause> {
        let inode = match self.target(path, mode)? {
            Target::File(inode) => inode,
            Target::New { dir, name, len } => {
                self.take(len)?;
                self.dir_mut(dir).entries.insert(name.into(), key);
                let file = File {
                    len,
                    sectors: BTreeMap::new(),
                    dirty: BTreeSet::new(),
                    holds: 0,
                    writers: 0,
                    linked: true,
                    durable: false,
                };
                self.inodes.insert(key, Inode::File(file));
                key
            }
        };
        let writable = mode != Mode::Read;
        // A crash between the create and the allocation leaves an empty file on `os`.
        if let Mode::Create { len } = mode
            && self.file(inode).len == 0
        {
            if let Err(cause) = self.take(len) {
                self.remove(path)?;
                return Err(cause);
            }
            self.file(inode).len = len;
        }
        let len = self.file(inode).len;
        let handle = Handle {
            inode,
            writable,
            key,
        };
        self.hold(handle);
        Ok((handle, len))
    }

    /// The names in the directory at `path`.
    pub(crate) fn list(&mut self, path: &Path) -> Result<Vec<PathBuf>, Cause> {
        let dir = self.dir(&segments(path))?;
        Ok(self
            .dir_mut(dir)
            .entries
            .keys()
            .map(PathBuf::from)
            .collect())
    }

    /// Makes the directory at `path`, with the key `key`.
    pub(crate) fn create_dir(&mut self, key: u64, path: &Path) -> Result<(), Cause> {
        let segments = segments(path);
        let Some((name, parent)) = segments.split_last() else {
            return Ok(());
        };
        let dir = self.dir(parent)?;
        let found = self.dir_mut(dir).entries.get(*name).copied();
        match found.map(|inode| &self.inodes[&inode]) {
            Some(Inode::Dir(_)) => Ok(()),
            Some(Inode::File(_)) => Err(Cause::Code(EXISTS)),
            None => {
                self.take(DIR_BYTES)?;
                self.dir_mut(dir).entries.insert(name.into(), key);
                self.inodes.insert(key, Inode::Dir(Dir::default()));
                Ok(())
            }
        }
    }

    /// Unlinks the file at `path`. It stays while a hold remains.
    pub(crate) fn remove(&mut self, path: &Path) -> Result<(), Cause> {
        let (segments, slashed) = (segments(path), slashed(path));
        let Some((name, parent)) = segments.split_last() else {
            return Err(Cause::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = *self
            .dir_mut(dir)
            .entries
            .get(*name)
            .ok_or(Cause::NotFound)?;
        self.named(inode, slashed)?.linked = false;
        self.dir_mut(dir).entries.remove(*name);
        self.collect(inode);
        Ok(())
    }

    /// Moves the entry of file `inode` from `from` to `to`, both in one directory.
    /// `NotFound` when `from` no longer names it; `Exists` when `to` is taken.
    pub(crate) fn rename(
        &mut self,
        inode: u64,
        from: &Path,
        to: &Path,
    ) -> Result<(), Cause> {
        let from = segments(from);
        let (old, parent) =
            from.split_last().expect("invariant: a rename is of a file");
        let new = segments(to)
            .pop()
            .expect("invariant: a rename is to a name");
        let dir = self.dir_mut(self.dir(parent)?);
        if dir.entries.get(*old) != Some(&inode) {
            return Err(Cause::NotFound);
        }
        if dir.entries.contains_key(new) {
            return Err(Cause::Exists(to.to_path_buf()));
        }
        dir.entries.remove(*old);
        dir.entries.insert(new.to_owned(), inode);
        Ok(())
    }

    /// Adds one hold of the file of `handle`.
    pub(crate) fn hold(&mut self, handle: Handle) {
        let file = self.file(handle.inode);
        file.holds += 1;
        file.writers += u64::from(handle.writable);
    }

    /// Drops one hold of the file of `handle`.
    pub(crate) fn release(&mut self, handle: Handle) {
        let file = self.file(handle.inode);
        file.holds -= 1;
        file.writers -= u64::from(handle.writable);
        self.collect(handle.inode);
    }

    /// Crashes the disk by `crash`. Each hold drops, as at the death of the process
    /// that held the files, and each file that only a hold kept is freed. After a
    /// `Power` crash, each directory goes back to its durable entries, what they no
    /// longer reach is freed, and each sector keeps its durable bytes or its bytes
    /// after one write that no sync covered, by `rng`.
    pub(crate) fn crash(&mut self, crash: Crash, rng: &mut Rng) {
        let inodes: Vec<u64> = self.inodes.keys().copied().collect();
        for inode in inodes {
            if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
                (file.holds, file.writers) = (0, 0);
                self.collect(inode);
            }
        }
        if crash == Crash::Power {
            self.cut_power(rng);
        }
    }

    /// Frees file `inode` when no entry, no durable entry, and no hold keeps it.
    fn collect(&mut self, inode: u64) {
        let file = self.file(inode);
        if !file.linked && !file.durable && file.holds == 0 {
            self.used -= file.len;
            self.inodes.remove(&inode);
        }
    }

    /// Makes the entries of the directory at `path` durable, and frees each file that
    /// only its old durable entries kept.
    pub(crate) fn sync_dir(&mut self, path: &Path) -> Result<(), Cause> {
        let key = self.dir(&segments(path))?;
        let dir = self.dir_mut(key);
        let old = mem::replace(&mut dir.durable, dir.entries.clone());
        let new: BTreeSet<u64> = dir.durable.values().copied().collect();
        for &inode in &new {
            if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
                file.durable = true;
            }
        }
        for inode in old.into_values().filter(|inode| !new.contains(inode)) {
            self.forget(inode);
        }
        Ok(())
    }

    /// Ends the durable entry that kept `inode`, and frees it when it is a file that
    /// nothing else keeps.
    fn forget(&mut self, inode: u64) {
        if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
            file.durable = false;
            self.collect(inode);
        }
    }

    /// Whether a [`Mode::Create`] open of `path` makes its file when the disk has
    /// room: the open succeeds, and finds no entry or a file with no bytes.
    pub(crate) fn makes(&self, path: &Path) -> bool {
        match self.target(path, Mode::Create { len: 0 }) {
            Ok(Target::New { .. }) => true,
            Ok(Target::File(inode)) => {
                matches!(&self.inodes[&inode], Inode::File(file) if file.len == 0)
            }
            Err(_) => false,
        }
    }

    /// What an open of `path` by `mode` finds, with each fault it gives before it
    /// takes space.
    fn target<'a>(&self, path: &'a Path, mode: Mode) -> Result<Target<'a>, Cause> {
        let (segments, slashed) = (segments(path), slashed(path));
        let Some((name, parent)) = segments.split_last() else {
            return Err(Cause::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let Inode::Dir(Dir { entries, .. }) = &self.inodes[&dir] else {
            unreachable!("invariant: inode {dir} is a directory");
        };
        let inode = match (entries.get(*name), mode) {
            (_, Mode::Create { .. }) if slashed => return Err(Cause::Code(DIRECTORY)),
            (Some(&inode), _) => inode,
            (None, Mode::Create { len }) => return Ok(Target::New { dir, name, len }),
            (None, Mode::Read | Mode::Write) => return Err(Cause::NotFound),
        };
        match &self.inodes[&inode] {
            Inode::File(_) if slashed => Err(Cause::Code(NOT_DIRECTORY)),
            Inode::File(file) if mode != Mode::Read && file.writers > 0 => {
                Err(Cause::Busy)
            }
            Inode::File(_) => Ok(Target::File(inode)),
            Inode::Dir(_) => Err(Cause::Code(DIRECTORY)),
        }
    }

    /// Makes the entry at `path` durable, and frees the file that its old durable entry
    /// kept when nothing else keeps it, as [`Disk::sync_dir`] does.
    pub(crate) fn commit(&mut self, path: &Path) {
        let segments = segments(path);
        let (name, parent) = segments.split_last().expect("invariant: a file path");
        let Ok(key) = self.dir(parent) else {
            unreachable!("invariant: an open made the file");
        };
        let dir = self.dir_mut(key);
        let inode = dir.entries[*name];
        let old = dir.durable.insert(name.into(), inode);
        self.file(inode).durable = true;
        if let Some(old) = old.filter(|&old| old != inode) {
            self.forget(old);
        }
    }

    /// Cuts the power, as [`Disk::crash`] says.
    fn cut_power(&mut self, rng: &mut Rng) {
        let mut reached = BTreeSet::from([ROOT]);
        let mut next = vec![ROOT];
        while let Some(at) = next.pop() {
            if let Some(Inode::Dir(dir)) = self.inodes.get_mut(&at) {
                dir.entries.clone_from(&dir.durable);
                reached.extend(dir.entries.values());
                next.extend(dir.entries.values());
            }
        }
        for (key, mut inode) in mem::take(&mut self.inodes) {
            if !reached.contains(&key) {
                self.used -= inode.bytes();
                continue;
            }
            if let Inode::File(file) = &mut inode {
                file.linked = true;
                file.cut_power(rng);
            }
            self.inodes.insert(key, inode);
        }
    }
}

impl Inode {
    /// The bytes that it takes.
    fn bytes(&self) -> u64 {
        match self {
            Self::File(file) => file.len,
            Self::Dir(_) => DIR_BYTES,
        }
    }
}

impl File {
    /// The bytes in `range`.
    pub(crate) fn bytes(&self, range: Range<u64>) -> Vec<u8> {
        let mut bytes = vec![0; index(range.end - range.start)];
        for (sector, part) in sectors(&range) {
            if let Some(found) = self.sectors.get(&sector) {
                let to = within(range.start, &part);
                let from = within(sector * SECTOR, &part);
                bytes[to].copy_from_slice(&found.last()[from]);
            }
        }
        bytes
    }

    /// Starts a read of `range`, and gives its bytes. First, the cache may drop the
    /// clean bytes that a failed sync lost in each sector of it, by `rng`.
    pub(crate) fn start_read(&mut self, range: Range<u64>, rng: &mut Rng) -> Vec<u8> {
        for (sector, _) in sectors(&range) {
            if let Some(found) = self.sectors.get_mut(&sector) {
                found.evict(rng);
            }
        }
        self.bytes(range)
    }

    /// Ends a write of `bytes` at `offset` that started at tick `started`. Each
    /// sector of a write whose future dropped keeps its bytes or takes the new ones
    /// by a coin. In each sector that takes them, the cache may first drop the clean
    /// bytes that a failed sync lost, by `rng`. Then, the write goes at a random place
    /// among the writes that ended since tick `started` and are not durable. Where it
    /// shares bytes with one of those, it goes in three parts, each at its own
    /// place: a random part of the bytes it shares with one of them, and the bytes
    /// before and after that part.
    pub(crate) fn write(
        &mut self,
        offset: u64,
        bytes: &[u8],
        started: u64,
        tick: u64,
        dropped: bool,
        rng: &mut Rng,
    ) {
        for (sector, part) in sectors(&(offset..offset + len(bytes))) {
            if dropped && rng.below(2) == 0 {
                continue;
            }
            let zeros = || Sector {
                durable: [0; env::files::SECTOR],
                clean: [0; env::files::SECTOR],
                writes: Vec::new(),
            };
            let found = self.sectors.entry(sector).or_insert_with(zeros);
            found.evict(rng);
            let first = (found.writes.iter())
                .rposition(|write| write.written < started)
                .map_or(0, |at| at + 1);
            let covered = within(sector * SECTOR, &part);
            let mut own = [0; env::files::SECTOR];
            own[covered.clone()].copy_from_slice(&bytes[within(offset, &part)]);
            let shared: Vec<Range<usize>> = (found.writes[first..].iter())
                .filter_map(|write| overlap(&write.covered, &covered))
                .collect();
            let parts = if shared.is_empty() {
                vec![covered]
            } else {
                let taken = piece(&shared[index(rng.below(len(&shared)))], rng);
                vec![
                    covered.start..taken.start,
                    taken.clone(),
                    taken.end..covered.end,
                ]
            };
            let choices = len(&found.writes[first..]) + 1;
            let mut placed: Vec<(usize, Write)> = (parts.into_iter())
                .filter(|part| !part.is_empty())
                .map(|part| {
                    let write = Write {
                        written: tick,
                        bytes: own[part.clone()].to_vec(),
                        covered: part,
                        after: [0; env::files::SECTOR],
                    };
                    (first + index(rng.below(choices)), write)
                })
                .collect();
            // Later places first, so that each place indexes the old order.
            placed.sort_by_key(|(place, _)| Reverse(*place));
            for (place, write) in placed {
                found.writes.insert(place, write);
            }
            found.replay(first);
            self.dirty.insert(sector);
        }
    }

    /// Makes durable, in each sector, each write that ended before tick `started`,
    /// and each write before it in the order.
    pub(crate) fn sync(&mut self, started: u64) {
        self.settle(started, |count| count - 1);
    }

    /// Makes durable, in each sector, the writes before a random place up to the last
    /// write that ended before tick `started`, in the order, by `rng`, as a sync that
    /// fails. The writes past that place up to there stay in the cache, clean, until
    /// the cache drops them: a read sees them, and a later write goes over them.
    pub(crate) fn tear(&mut self, started: u64, rng: &mut Rng) {
        self.settle(started, |count| rng.below(count));
    }

    /// Keeps in each sector its durable bytes or its bytes after one write that no
    /// sync covered, by `rng`, and empties the cache, as a power cut does.
    fn cut_power(&mut self, rng: &mut Rng) {
        self.settle(u64::MAX, |count| rng.below(count));
        for sector in self.sectors.values_mut() {
            sector.clean = sector.durable;
        }
    }

    /// In each sector, makes durable the first writes up to the last that ended
    /// before tick `started`, as many as `pick` gives for the count of choices, and
    /// leaves the cache with each write up to there.
    fn settle(&mut self, started: u64, mut pick: impl FnMut(u64) -> u64) {
        for sector in mem::take(&mut self.dirty) {
            let found = (self.sectors.get_mut(&sector))
                .expect("invariant: a dirty sector is written");
            let writes = &mut found.writes;
            if let Some(last) = writes.iter().rposition(|write| write.written < started)
            {
                let kept = index(pick(len(&writes[..=last]) + 1));
                if let Some(durable) = kept.checked_sub(1) {
                    found.durable = writes[durable].after;
                }
                found.clean = writes[last].after;
                writes.drain(..=last);
            }
            if !found.writes.is_empty() {
                self.dirty.insert(sector);
            }
        }
    }

    /// Ends a read of `range`, whose bytes were `before` when it started, while
    /// `writes` (offset and bytes) are in flight on the file. Each sector of the
    /// range takes its bytes at the start, its bytes now, or the bytes of one of
    /// those writes over it, and then one of these over a random part of it, by the
    /// run's disk stream.
    pub(crate) fn read(
        &self,
        range: Range<u64>,
        before: &[u8],
        writes: &[(u64, &[u8])],
        rng: &mut Rng,
    ) -> Vec<u8> {
        let now = self.bytes(range.clone());
        let mut bytes = now.clone();
        for (_, part) in sectors(&range) {
            let over: Vec<(Range<usize>, &[u8])> = (writes.iter())
                .filter_map(|&(offset, write)| {
                    let common = overlap(&part, &(offset..offset + len(write)))?;
                    Some((
                        within(range.start, &common),
                        &write[within(offset, &common)],
                    ))
                })
                .collect();
            let choices = len(&over) + 2;
            let sector = within(range.start, &part);
            let first = rng.below(choices);
            let (second, run) = (rng.below(choices), piece(&sector, rng));
            for (pick, run) in [(first, sector), (second, run)] {
                match pick {
                    0 => bytes[run.clone()].copy_from_slice(&before[run]),
                    1 => bytes[run.clone()].copy_from_slice(&now[run]),
                    pick => {
                        let (common, write) = &over[index(pick - 2)];
                        if let Some(run) = overlap(common, &run) {
                            let skip = run.start - common.start;
                            bytes[run.clone()]
                                .copy_from_slice(&write[skip..][..run.len()]);
                        }
                    }
                }
            }
        }
        bytes
    }
}

impl Sector {
    /// The bytes that a read sees.
    fn last(&self) -> &[u8; env::files::SECTOR] {
        self.writes.last().map_or(&self.clean, |write| &write.after)
    }

    /// Drops the clean bytes that a failed sync lost from the cache, by a coin, as
    /// Linux may evict a clean page. A sector with a write is dirty, so it keeps them.
    fn evict(&mut self, rng: &mut Rng) {
        if self.writes.is_empty() && self.clean != self.durable {
            self.clean = [self.clean, self.durable][index(rng.below(2))];
        }
    }

    /// Puts each write from place `from` on over the sector before it.
    fn replay(&mut self, from: usize) {
        for at in from..self.writes.len() {
            let (before, rest) = self.writes.split_at_mut(at);
            let mut after = before.last().map_or(self.clean, |write| write.after);
            let write = &mut rest[0];
            after[write.covered.clone()].copy_from_slice(&write.bytes);
            write.after = after;
        }
    }
}

/// The positions that `a` and `b` share, or `None` when they share none.
fn overlap<T: Ord + Copy>(a: &Range<T>, b: &Range<T>) -> Option<Range<T>> {
    let (start, end) = (a.start.max(b.start), a.end.min(b.end));
    (start < end).then_some(start..end)
}

/// A random part of `range` between two cuts, by `rng`: from none of it to all of it.
fn piece(range: &Range<usize>, rng: &mut Rng) -> Range<usize> {
    let count = u64::try_from(range.len()).expect("invariant: usize fits u64");
    let mut cut = || range.start + index(rng.below(count + 1));
    let (a, b) = (cut(), cut());
    a.min(b)..a.max(b)
}

/// Each sector that holds a byte of `range`, with the part of `range` in it.
fn sectors(range: &Range<u64>) -> impl Iterator<Item = (u64, Range<u64>)> {
    (range.start / SECTOR..range.end.div_ceil(SECTOR)).filter_map(|sector| {
        let start = sector * SECTOR;
        let part = overlap(&(start..start.saturating_add(SECTOR)), range)?;
        Some((sector, part))
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

/// Whether `path` ends in `/` or `/.`, as `a/` does. Such a path names only a
/// directory.
fn slashed(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

/// The path of the same file as `path`, with only its names.
pub(crate) fn normal(path: &Path) -> PathBuf {
    segments(path).into_iter().collect()
}

fn index(at: u64) -> usize {
    usize::try_from(at).expect("invariant: a position in memory fits usize")
}

fn len<T>(items: &[T]) -> u64 {
    u64::try_from(items.len()).expect("invariant: usize fits u64")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> File {
        File {
            len: 2 * SECTOR,
            sectors: BTreeMap::new(),
            dirty: BTreeSet::new(),
            holds: 0,
            writers: 0,
            linked: true,
            durable: false,
        }
    }

    #[test]
    fn a_write_that_ends_at_the_tick_a_sync_starts_is_not_durable() {
        let kept: BTreeSet<Vec<u8>> = (0..64)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = file();
                file.write(0, &[1; 512], 1, 2, false, &mut rng);
                file.sync(2);
                file.cut_power(&mut rng);
                file.bytes(0..SECTOR)
            })
            .collect();
        assert_eq!(kept, BTreeSet::from([vec![0; 512], vec![1; 512]]));
    }

    #[test]
    fn a_write_that_starts_at_the_tick_another_ends_may_go_before_it() {
        let last: BTreeSet<Vec<u8>> = (0..64)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = file();
                file.write(0, &[1; 512], 1, 2, false, &mut rng);
                file.write(0, &[2; 512], 2, 3, false, &mut rng);
                file.bytes(0..SECTOR)
            })
            .collect();
        assert!(last.iter().flatten().all(|byte| [1, 2].contains(byte)));
        let whole = (last.iter()).filter(|bytes| bytes.iter().all(|&b| b == bytes[0]));
        let whole: BTreeSet<&Vec<u8>> = whole.collect();
        assert_eq!(whole, BTreeSet::from([&vec![1; 512], &vec![2; 512]]));
    }

    #[test]
    fn a_read_after_a_sync_that_fails_sees_each_write() {
        for seed in 0..64 {
            let mut rng = Rng::from_seed(seed);
            let mut file = file();
            file.write(256, &[1; 256], 2, 3, false, &mut rng);
            file.write(0, &[2; 256], 1, 5, false, &mut rng);
            file.tear(4, &mut rng);
            let expected = [[2; 256], [1; 256]].concat();
            assert_eq!(file.bytes(0..SECTOR), expected, "seed {seed}");
        }
    }

    #[test]
    fn a_sync_that_fails_leaves_dirty_a_write_that_ended_after_it_started() {
        for seed in 0..64 {
            let mut rng = Rng::from_seed(seed);
            let mut file = file();
            file.write(0, &[1; 512], 1, 2, false, &mut rng);
            file.write(0, &[2; 512], 4, 5, false, &mut rng);
            file.tear(3, &mut rng);
            file.sync(6);
            file.cut_power(&mut rng);
            assert_eq!(file.bytes(0..SECTOR), vec![2; 512], "seed {seed}");
        }
    }

    /// A file whose sector 0 holds 1s clean in the cache, over durable 0s, after a
    /// write and a sync that failed.
    fn create_lost(rng: &mut Rng) -> File {
        let mut file = file();
        file.write(0, &[1; 512], 1, 2, false, rng);
        file.settle(3, |_| 0);
        file
    }

    #[test]
    fn a_read_after_a_failed_sync_sees_the_lost_or_the_durable_bytes() {
        let seen: BTreeSet<Vec<u8>> = (0..64)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = create_lost(&mut rng);
                let started = file.start_read(0..SECTOR, &mut rng);
                assert_eq!(file.bytes(0..SECTOR), started, "seed {seed}");
                started
            })
            .collect();
        assert_eq!(seen, BTreeSet::from([vec![0; 512], vec![1; 512]]));
    }

    #[test]
    fn a_write_after_a_failed_sync_goes_over_the_lost_or_the_durable_bytes() {
        let kept: BTreeSet<Vec<u8>> = (0..64)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = create_lost(&mut rng);
                file.write(0, &[3], 4, 5, false, &mut rng);
                file.sync(6);
                file.cut_power(&mut rng);
                file.bytes(0..SECTOR)
            })
            .collect();
        let over = |byte| [[3].as_slice(), &[byte; 511]].concat();
        assert_eq!(kept, BTreeSet::from([over(0), over(1)]));
    }

    #[test]
    fn the_cache_keeps_the_lost_bytes_under_a_write_that_no_sync_covered() {
        for seed in 0..64 {
            let mut rng = Rng::from_seed(seed);
            let mut file = file();
            file.write(0, &[1; 512], 1, 2, false, &mut rng);
            file.write(256, &[2; 256], 3, 5, false, &mut rng);
            file.settle(4, |_| 0);
            file.start_read(0..SECTOR, &mut rng);
            file.write(0, &[3; 128], 4, 6, false, &mut rng);
            assert_eq!(file.bytes(128..256), vec![1; 128], "seed {seed}");
        }
    }

    #[test]
    fn a_write_in_flight_never_brings_back_bytes_that_a_later_write_covered() {
        for (len, seed) in [100, 512]
            .into_iter()
            .flat_map(|len| (0..256).map(move |seed| (len, seed)))
        {
            let mut rng = Rng::from_seed(seed);
            let mut file = file();
            file.write(0, &vec![2; len], 2, 3, false, &mut rng);
            file.write(0, &vec![3; len], 4, 5, false, &mut rng);
            file.write(0, &[1; 512], 1, 6, false, &mut rng);
            assert!(!file.bytes(0..SECTOR).contains(&2), "{len} {seed}");
        }
    }

    #[test]
    fn a_power_cut_may_keep_a_part_of_a_write_that_overlapped_another() {
        let kept: Vec<Vec<u8>> = (0..256)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = file();
                file.write(0, &[2; 512], 1, 2, false, &mut rng);
                file.write(0, &[1; 512], 1, 3, false, &mut rng);
                file.cut_power(&mut rng);
                file.bytes(0..SECTOR)
            })
            .collect();
        assert!(kept.iter().flatten().all(|byte| [0, 1, 2].contains(byte)));
        let part = (kept.iter()).any(|bytes| bytes.contains(&0) && bytes.contains(&1));
        assert!(part, "{kept:?}");
    }

    #[test]
    fn a_piece_can_be_each_part_of_its_range() {
        let mut rng = Rng::from_seed(0);
        let pieces: BTreeSet<(usize, usize)> = (0..64)
            .map(|_| piece(&(3..5), &mut rng))
            .map(|part| (part.start, part.end))
            .collect();
        let parts = [(3, 3), (3, 4), (3, 5), (4, 4), (4, 5), (5, 5)];
        assert_eq!(pieces, BTreeSet::from(parts));
    }

    #[test]
    fn a_sync_leaves_dirty_only_the_sectors_with_a_later_write() {
        let mut rng = Rng::from_seed(0);
        let mut file = file();
        file.write(0, &[1; 1024], 1, 2, false, &mut rng);
        file.write(SECTOR, &[2; 512], 3, 5, false, &mut rng);
        file.sync(4);
        assert_eq!(file.dirty, BTreeSet::from([1]));
    }
}
