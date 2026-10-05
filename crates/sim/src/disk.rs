//! The file system of one node: directories, and sparse files of 512-byte sectors.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::mem;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use env::files::Mode;
use env::rng::Rng;

/// The key of the data directory.
pub(crate) const ROOT: u64 = 0;
/// The bytes of a sector: a write keeps or loses each sector whole.
const SECTOR: u64 = 512;
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
    Code(i32),
}

pub(crate) struct Disk {
    bytes: u64,
    /// The bytes of the directories, and of the files that a directory entry names
    /// or a hold keeps.
    used: u64,
    inodes: BTreeMap<u64, Inode>,
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
    /// The sectors with more than one version.
    dirty: BTreeSet<u64>,
    /// The descriptors and the calls in flight that use the file.
    holds: u64,
    /// An entry names the file.
    linked: bool,
    /// A durable entry names the file.
    durable: bool,
}

/// The durable bytes of a sector, then its bytes after each write on it since
/// then, in order. A read sees the last.
struct Sector(Vec<Version>);

struct Version {
    bytes: [u8; 512],
    /// The tick at which its write ended.
    written: u64,
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
    pub(crate) fn dir(&self, segments: &[&OsStr]) -> Result<u64, Cause> {
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

    /// Opens the file at `segments`, and gives its key and length. The open holds
    /// it. A file that it makes takes the key `key`.
    pub(crate) fn open(
        &mut self,
        key: u64,
        segments: &[&OsStr],
        mode: Mode,
    ) -> Result<(u64, u64), Cause> {
        let Some((name, parent)) = segments.split_last() else {
            return Err(Cause::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = match (self.dir_mut(dir).entries.get(*name).copied(), mode) {
            (Some(inode), _) => inode,
            (None, Mode::Create { len }) => {
                self.take(len)?;
                self.dir_mut(dir).entries.insert(name.into(), key);
                let file = File {
                    len,
                    sectors: BTreeMap::new(),
                    dirty: BTreeSet::new(),
                    holds: 0,
                    linked: true,
                    durable: false,
                };
                self.inodes.insert(key, Inode::File(file));
                key
            }
            (None, Mode::Read | Mode::Write) => return Err(Cause::NotFound),
        };
        let Some(Inode::File(file)) = self.inodes.get_mut(&inode) else {
            return Err(Cause::Code(DIRECTORY));
        };
        file.holds += 1;
        Ok((inode, file.len))
    }

    /// The names in the directory at `segments`.
    pub(crate) fn list(&mut self, segments: &[&OsStr]) -> Result<Vec<PathBuf>, Cause> {
        let dir = self.dir(segments)?;
        Ok(self
            .dir_mut(dir)
            .entries
            .keys()
            .map(PathBuf::from)
            .collect())
    }

    /// Makes the directory at `segments`, with the key `key`.
    pub(crate) fn create_dir(
        &mut self,
        key: u64,
        segments: &[&OsStr],
    ) -> Result<(), Cause> {
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

    /// Unlinks the file at `segments`. It stays while a hold remains.
    pub(crate) fn remove(&mut self, segments: &[&OsStr]) -> Result<(), Cause> {
        let Some((name, parent)) = segments.split_last() else {
            return Err(Cause::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = *self
            .dir_mut(dir)
            .entries
            .get(*name)
            .ok_or(Cause::NotFound)?;
        let Some(Inode::File(file)) = self.inodes.get_mut(&inode) else {
            return Err(Cause::Code(DIRECTORY));
        };
        file.linked = false;
        self.dir_mut(dir).entries.remove(*name);
        self.collect(inode);
        Ok(())
    }

    /// Adds one hold of open file `inode`.
    pub(crate) fn hold(&mut self, inode: u64) {
        self.file(inode).holds += 1;
    }

    /// Drops one hold of file `inode`.
    pub(crate) fn release(&mut self, inode: u64) {
        self.file(inode).holds -= 1;
        self.collect(inode);
    }

    /// Frees file `inode` when no entry, no durable entry, and no hold keeps it.
    fn collect(&mut self, inode: u64) {
        let file = self.file(inode);
        if !file.linked && !file.durable && file.holds == 0 {
            self.used -= file.len;
            self.inodes.remove(&inode);
        }
    }

    /// Makes the entries of the directory at `segments` durable, and frees each file
    /// that only its old durable entries kept.
    pub(crate) fn sync_dir(&mut self, segments: &[&OsStr]) -> Result<(), Cause> {
        let key = self.dir(segments)?;
        let dir = self.dir_mut(key);
        let old = mem::replace(&mut dir.durable, dir.entries.clone());
        let new: BTreeSet<u64> = dir.durable.values().copied().collect();
        for &inode in &new {
            if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
                file.durable = true;
            }
        }
        for inode in old.into_values().filter(|inode| !new.contains(inode)) {
            if let Some(Inode::File(file)) = self.inodes.get_mut(&inode) {
                file.durable = false;
                self.collect(inode);
            }
        }
        Ok(())
    }

    /// Cuts the power: each directory goes back to its durable entries, what they no
    /// longer reach is freed, and each sector keeps its durable bytes or those of
    /// one write since then, by `rng`. No descriptor and no call in flight may hold
    /// a file.
    pub(crate) fn cut_power(&mut self, rng: &mut Rng) {
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
                file.settle(u64::MAX, |count| rng.below(count));
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
                bytes[to].copy_from_slice(&found.last().bytes[from]);
            }
        }
        bytes
    }

    /// Ends a write of `bytes` at `offset` that started at tick `started`. Each
    /// sector that another write ended on since then, and each sector of a write
    /// whose future dropped, keeps its bytes or takes the new ones by a coin. Each
    /// sector that takes them gets a new version.
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
            let found = self.sectors.get(&sector);
            let raced = found.is_some_and(|found| found.last().written > started);
            if (raced || dropped) && rng.below(2) == 0 {
                continue;
            }
            let zeros = || {
                Sector(vec![Version {
                    bytes: [0; 512],
                    written: 0,
                }])
            };
            let found = self.sectors.entry(sector).or_insert_with(zeros);
            let mut new = found.last().bytes;
            new[within(sector * SECTOR, &part)]
                .copy_from_slice(&bytes[within(offset, &part)]);
            found.0.push(Version {
                bytes: new,
                written: tick,
            });
            self.dirty.insert(sector);
        }
    }

    /// Makes durable, in each sector, the last version of a write that ended before
    /// tick `started`.
    pub(crate) fn sync(&mut self, started: u64) {
        self.settle(started, |count| count - 1);
    }

    /// Makes durable, in each sector, its durable bytes or the bytes of a write that
    /// ended before tick `started`, by `rng`, as a sync that fails.
    pub(crate) fn tear(&mut self, started: u64, rng: &mut Rng) {
        self.settle(started, |count| rng.below(count));
    }

    /// Makes one version of each sector durable and visible: of the durable version
    /// and the versions of the writes that ended before tick `started`, the one that
    /// `pick` gives for their count. The versions of later writes stay.
    fn settle(&mut self, started: u64, mut pick: impl FnMut(u64) -> u64) {
        for sector in mem::take(&mut self.dirty) {
            let Sector(versions) = (self.sectors.get_mut(&sector))
                .expect("invariant: a dirty sector is written");
            let before = versions.partition_point(|version| version.written < started);
            if before > 1 {
                let count = u64::try_from(before).expect("invariant: usize fits u64");
                let kept = index(pick(count));
                versions.drain(kept + 1..before);
                versions.drain(..kept);
            }
            if versions.len() > 1 {
                self.dirty.insert(sector);
            }
        }
    }

    /// Ends a read of `range`, whose bytes were `before` when it started, while
    /// `writes` (offset and bytes) are in flight on the file. Each sector of the
    /// range takes its bytes at the start, its bytes now, or the bytes of one of
    /// those writes over it, by the run's disk stream.
    pub(crate) fn read(
        &self,
        range: Range<u64>,
        before: &[u8],
        writes: &[(u64, &[u8])],
        rng: &mut Rng,
    ) -> Vec<u8> {
        let mut bytes = self.bytes(range.clone());
        for (_, part) in sectors(&range) {
            let over: Vec<(u64, &[u8], Range<u64>)> = (writes.iter())
                .filter_map(|&(offset, write)| {
                    let common = overlap(&part, &(offset..offset + len(write)))?;
                    Some((offset, write, common))
                })
                .collect();
            let choices =
                u64::try_from(over.len() + 2).expect("invariant: usize fits u64");
            let to = within(range.start, &part);
            match rng.below(choices) {
                0 => bytes[to.clone()].copy_from_slice(&before[to]),
                1 => {}
                pick => {
                    let (offset, write, common) = &over[index(pick - 2)];
                    let to = within(range.start, common);
                    bytes[to].copy_from_slice(&write[within(*offset, common)]);
                }
            }
        }
        bytes
    }
}

impl Sector {
    /// The bytes that a read sees.
    fn last(&self) -> &Version {
        self.0.last().expect("invariant: a sector has a version")
    }
}

/// The bytes that `a` and `b` share, or `None` when they share none.
fn overlap(a: &Range<u64>, b: &Range<u64>) -> Option<Range<u64>> {
    let (start, end) = (a.start.max(b.start), a.end.min(b.end));
    (start < end).then_some(start..end)
}

/// Each sector that holds a byte of `range`, with the part of `range` in it.
fn sectors(range: &Range<u64>) -> impl Iterator<Item = (u64, Range<u64>)> {
    (range.start / SECTOR..range.end.div_ceil(SECTOR)).filter_map(|sector| {
        let part = overlap(&(sector * SECTOR..(sector + 1) * SECTOR), range)?;
        Some((sector, part))
    })
}

/// `part` as indexes into a buffer that starts at `start`.
fn within(start: u64, part: &Range<u64>) -> Range<usize> {
    index(part.start - start)..index(part.end - start)
}

/// The segments of a checked path: only its names, since `.` adds nothing.
pub(crate) fn segments(path: &Path) -> Vec<&OsStr> {
    (path.components())
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// The path of the same file as `path`, with only its names.
pub(crate) fn normal(path: &Path) -> PathBuf {
    segments(path).into_iter().collect()
}

fn index(at: u64) -> usize {
    usize::try_from(at).expect("invariant: a position in memory fits usize")
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("invariant: usize fits u64")
}
