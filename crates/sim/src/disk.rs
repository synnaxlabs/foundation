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

/// The durable bytes of a sector, and the writes on it since then in one order that
/// the times of their calls allow.
struct Sector {
    durable: [u8; 512],
    writes: Vec<Write>,
}

/// One write on a sector.
struct Write {
    /// The tick at which it ended.
    written: u64,
    /// The bytes of the sector that it covered.
    covered: Range<usize>,
    /// Its bytes over `covered`.
    bytes: Vec<u8>,
    /// The sector after it and each write before it.
    after: [u8; 512],
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

    /// Opens the file at `path`, and gives its key and length. The open holds it. A
    /// file that it makes takes the key `key`.
    pub(crate) fn open(
        &mut self,
        key: u64,
        path: &Path,
        mode: Mode,
    ) -> Result<(u64, u64), Cause> {
        let (segments, slashed) = (segments(path), slashed(path));
        let Some((name, parent)) = segments.split_last() else {
            return Err(Cause::Code(DIRECTORY));
        };
        let dir = self.dir(parent)?;
        let inode = match (self.dir_mut(dir).entries.get(*name).copied(), mode) {
            (_, Mode::Create { .. }) if slashed => return Err(Cause::Code(DIRECTORY)),
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
        let file = self.named(inode, slashed)?;
        file.holds += 1;
        Ok((inode, file.len))
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
                bytes[to].copy_from_slice(&found.last()[from]);
            }
        }
        bytes
    }

    /// Ends a write of `bytes` at `offset` that started at tick `started`. Each
    /// sector of a write whose future dropped keeps its bytes or takes the new ones
    /// by a coin. In each sector that takes them, the write goes at a random place
    /// among the writes that ended since tick `started` and are not durable.
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
                durable: [0; 512],
                writes: Vec::new(),
            };
            let found = self.sectors.entry(sector).or_insert_with(zeros);
            let first = (found.writes.iter())
                .rposition(|write| write.written < started)
                .map_or(0, |at| at + 1);
            let place = first + index(rng.below(len(&found.writes[first..]) + 1));
            let write = Write {
                written: tick,
                covered: within(sector * SECTOR, &part),
                bytes: bytes[within(offset, &part)].to_vec(),
                after: [0; 512],
            };
            found.writes.insert(place, write);
            found.replay(place);
            self.dirty.insert(sector);
        }
    }

    /// Makes durable, in each sector, each write that ended before tick `started`,
    /// and each write before it in the order.
    pub(crate) fn sync(&mut self, started: u64) {
        self.settle(started, |count| count - 1);
    }

    /// Makes durable, in each sector, the writes before a random place up to the last
    /// write that ended before tick `started`, by `rng`, as a sync that fails. The
    /// writes past that place that ended before tick `started` are lost.
    pub(crate) fn tear(&mut self, started: u64, rng: &mut Rng) {
        self.settle(started, |count| rng.below(count));
    }

    /// In each sector, makes durable the first writes up to the last that ended
    /// before tick `started`, as many as `pick` gives for the count of choices, and
    /// drops the others up to there that ended before tick `started`.
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
                *writes = (mem::take(writes).into_iter().enumerate())
                    .filter(|(at, write)| {
                        *at > last || (*at >= kept && write.written >= started)
                    })
                    .map(|(_, write)| write)
                    .collect();
                found.replay(0);
            }
            if !found.writes.is_empty() {
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
    fn last(&self) -> &[u8; 512] {
        self.writes
            .last()
            .map_or(&self.durable, |write| &write.after)
    }

    /// Puts each write from place `from` on over the sector before it.
    fn replay(&mut self, from: usize) {
        for at in from..self.writes.len() {
            let (before, rest) = self.writes.split_at_mut(at);
            let mut after = before.last().map_or(self.durable, |write| write.after);
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
                // As at a power cut: keep one version that is still in play.
                file.tear(u64::MAX, &mut rng);
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
        assert_eq!(last, BTreeSet::from([vec![1; 512], vec![2; 512]]));
    }

    #[test]
    fn a_sync_that_fails_keeps_each_write_that_ended_after_it_started() {
        let kept: BTreeSet<Vec<u8>> = (0..64)
            .map(|seed| {
                let mut rng = Rng::from_seed(seed);
                let mut file = file();
                file.write(256, &[1; 256], 2, 3, false, &mut rng);
                file.write(0, &[2; 256], 1, 5, false, &mut rng);
                file.tear(4, &mut rng);
                file.bytes(0..SECTOR)
            })
            .collect();
        let expected = [[[2; 256], [0; 256]].concat(), [[2; 256], [1; 256]].concat()];
        assert_eq!(kept, BTreeSet::from(expected));
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
