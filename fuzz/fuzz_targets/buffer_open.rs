//! `Buffer::open` never panics on a ring file that a local writer changed, and what
//! `committed` reported durable is what a reopen gives.
//!
//! `Reader::read` gives each path of a ring that opens as its doc says: every entry
//! up to the tail, a gap only for seqs that no entry holds, each entry that its
//! budget takes, and the same entries in one read, in reads of a small budget, from
//! inside an entry or a gap, and after a reopen. A carry record can give a path a
//! tail past its entries, with a stamp that no entry gives: a read then gives the
//! seqs up to the tail as a gap with no entry.
//!
//! Input: batches that the production path writes, then edits on the file bytes.
//! A batch makes a record of one to four blocks, with a table of up to four.
//! Two edits seal a CRC: a header block and a record. One edit writes a tail and
//! its chain value in the header block that the open reads: a block of the area, on
//! one of the first 128 laps or the last 128 that hold the block.
//!
//! The target follows the records of the ring ([`Room`]) while no edit other than a
//! move of the tail changed it, with the trim and the carry record of each commit and
//! the trim of each open. Each append, open, and reopen must then fit, or give
//! `Full` with the bytes needed and free that the target expects, and a reopen must
//! give each entry of the records it keeps. An open whose records pass the last
//! offset must give `Invalid`.
//! The limits of 1023 entries and 1023 parts are not reached, as a batch has at most
//! 259 entries: `checks_a_batch_against_each_limit_at_its_boundary` in `wal` and
//! `an_append_is_large_exactly_when_the_layout_check_fails` in the `buffer` tests own
//! them.
//!
//! The edits restate the formats in `header.rs` and `record.rs` of `buffer`. Before
//! it edits, the target walks the build's ring from the tail. Each record must have
//! a kind and seal as written, the records must end where the room of the build
//! ends, and a seal of each record and a write of the tail where it is must change
//! nothing. So when a format moves, the target panics.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::VecDeque;
use std::iter;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Heap, Pool};
use buffer::{
    Buffer, Config, Entry, Error, Layout, Limit, Parts, Rejected, Samples, Stored, Tail,
};
use env::files::{File, Mode};
use env::tasks::Tasks;
use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use types::channel::{self, Slot, Slots};
use types::frame;
use types::log::Mark;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

const BLOCK: usize = 4096;
/// The largest record, in blocks.
const RECORD_BLOCKS: usize = 4;
/// Blocks in the area: four of the largest record and the three blocks that each
/// record leaves free, the least area of a ring.
const BLOCKS: usize = 4 * RECORD_BLOCKS + 3;
const AREA: u64 = (BLOCKS * BLOCK) as u64;
/// The first block of the area: the two header blocks come before it.
const AREA_START: usize = 2;
const BODY_MAX: usize = RECORD_BLOCKS * BLOCK - record::HEAD;
/// The most bytes of one decoded entry. One such entry makes a record of three
/// blocks, and two make a batch over the body limit.
const PART_MAX: usize = 2 * BLOCK - 1;
const FILE_LEN: usize = (AREA_START + BLOCKS) * BLOCK;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const COMMIT: Span = Span::from_nanos(10_000_000);
const INDEXES: usize = 3;
/// The least and the most bytes of each entry the check commits: a record of one
/// block, and one of the largest.
const CHECK_PART: usize = 512;
const CHECK_PART_MAX: usize = 2677;
const _: () = assert!(check_record(CHECK_PART) <= BLOCK);
const _: () = assert!(check_record(CHECK_PART_MAX) <= RECORD_BLOCKS * BLOCK);
const PATHS: [frame::Path; 2] = [frame::Path::Live, frame::Path::Backfill];

/// The bytes of the check's record with entries of `part` bytes.
const fn check_record(part: usize) -> usize {
    let entries = INDEXES * PATHS.len();
    record::HEAD + body(entries, entries * part)
}

/// The body of a record of `entries` entries and `bytes` bytes: 4 bytes and 51 an
/// entry of table, then the bytes.
const fn body(entries: usize, bytes: usize) -> usize {
    4 + 51 * entries + bytes
}
/// A read budget that some entries of a path fill together.
const BUDGET: usize = 1024;
/// A key that no build writes. Its slot comes after each slot an open made.
const SPARE: channel::Key = channel::Key::from_u128(u128::MAX);
const CHECK_LAST: Stamp = Stamp::from_nanos(9);
/// The tag of the first entry of the build and of the check. Each entry takes the
/// next tag, fills its bytes with it, and is stored at it. The tags of the build
/// start again after 128 entries, below the tags of the check, so entries that
/// share a tag differ in `first`. A build entry over [`Layout::ENTRY_MAX_MIN`]
/// bytes is appended with tag 0, as a nonzero tag holds at most that.
const BUILD_TAG: u8 = 0x01;
const CHECK_TAG: u8 = 0x81;
/// The last offset of a ring.
const OFFSET_LAST: u128 = u64::MAX as u128;

/// The ring offset of `block` of the file on `lap`.
fn offset(block: usize, lap: u64) -> u64 {
    lap * AREA + ((block - AREA_START) * BLOCK) as u64
}

/// The last lap whose `block` of the file a `u64` offset holds. Since the area is no
/// power of two, the first blocks have one more lap than the last.
fn lap_last(block: usize) -> u64 {
    (u64::MAX - offset(block, 0)) / AREA
}

/// The block of the file and the lap of ring offset `offset`: the inverse of
/// [`offset`] at a block boundary.
fn place(offset: u64) -> (usize, u64) {
    (AREA_START + (offset % AREA) as usize / BLOCK, offset / AREA)
}

fn word(image: &[u8], at: usize) -> u32 {
    let bytes = image[at..at + 4].try_into().expect("invariant: four bytes");
    u32::from_le_bytes(bytes)
}

/// The header blocks of the file, as `header.rs` of `buffer` writes them. Each
/// field function takes the start of one block.
mod header {
    use super::{BLOCK, word};

    const MAGIC: &[u8] = b"FNDNRING";
    const BODY_MAX_AT: usize = 18;
    const TAIL_AT: usize = 22;
    const CHAIN_AT: usize = 30;
    const SEQ_AT: usize = 34;
    /// The CRC, right after the fields.
    const CRC_AT: usize = 42;
    /// The CRC covers the first sector of a header block.
    const SECTOR: usize = 512;

    /// The start of the header block that the open reads: the newer whole one by
    /// its wrapped seq, the first on a tie, or none when neither is whole.
    pub(super) fn read(image: &[u8]) -> Option<usize> {
        let seq = |start: usize| {
            let bytes = &image[start + SEQ_AT..start + SEQ_AT + 8];
            u64::from_le_bytes(bytes.try_into().expect("invariant: eight bytes"))
        };
        match (whole(image, 0), whole(image, BLOCK)) {
            (true, true) if seq(BLOCK).wrapping_sub(seq(0)).cast_signed() > 0 => {
                Some(BLOCK)
            }
            (true, _) => Some(0),
            (false, true) => Some(BLOCK),
            (false, false) => None,
        }
    }

    /// The start of the header block that the open reads, or the first when neither
    /// is whole.
    pub(super) fn chosen(image: &[u8]) -> usize {
        read(image).unwrap_or(0)
    }

    /// Whether the header block at `start` has the magic and a right CRC.
    fn whole(image: &[u8], start: usize) -> bool {
        image[start..].starts_with(MAGIC)
            && word(image, start + CRC_AT) == crc(image, start)
    }

    /// The most bytes of a record body.
    pub(super) fn body_max(image: &[u8], start: usize) -> usize {
        let body_max = word(image, start + BODY_MAX_AT);
        usize::try_from(body_max).expect("invariant: a u32 fits")
    }

    /// The tail offset.
    pub(super) fn tail(image: &[u8], start: usize) -> u64 {
        let tail = image[start + TAIL_AT..start + TAIL_AT + 8]
            .try_into()
            .expect("invariant: eight bytes");
        u64::from_le_bytes(tail)
    }

    /// The chain value the record at the tail continues from.
    pub(super) fn chain(image: &[u8], start: usize) -> u32 {
        word(image, start + CHAIN_AT)
    }

    /// Writes the tail offset and its chain value, and seals the block.
    pub(super) fn put_tail(image: &mut [u8], start: usize, tail: u64, chain: u32) {
        let at = start + TAIL_AT;
        image[at..at + 8].copy_from_slice(&tail.to_le_bytes());
        let at = start + CHAIN_AT;
        image[at..at + 4].copy_from_slice(&chain.to_le_bytes());
        seal(image, start);
    }

    /// Seals the header block at `start`.
    pub(super) fn seal(image: &mut [u8], start: usize) {
        let at = start + CRC_AT;
        let crc = crc(image, start);
        image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// The CRC of the header block at `start`, over its first sector less the CRC.
    fn crc(image: &[u8], start: usize) -> u32 {
        let at = start + CRC_AT;
        let crc = crc32c::crc32c(&image[start..at]);
        crc32c::crc32c_append(crc, &image[at + 4..start + SECTOR])
    }
}

/// A record of the area, as `record.rs` of `buffer` writes it:
/// `[len: u32][crc32c: u32][kind: u8][body]`.
mod record {
    use super::{BLOCK, word};

    const CRC_AT: usize = 4;
    pub(super) const KIND_AT: usize = 8;
    pub(super) const HEAD: usize = 9;
    pub(super) const DATA: u8 = 1;
    pub(super) const WRAP: u8 = 2;
    pub(super) const RESTART: u8 = 3;
    pub(super) const CARRY: u8 = 4;

    pub(super) fn len(image: &[u8], block: usize) -> usize {
        usize::try_from(word(image, block * BLOCK)).expect("invariant: a u32 fits")
    }

    pub(super) fn kind(image: &[u8], block: usize) -> u8 {
        image[block * BLOCK + KIND_AT]
    }

    /// The index, the path byte, and the seq of each tail of the carry record at
    /// `block`, whose body is `[tails: u16][entries: u16]` and tails of `index: u128,
    /// path: u8, seq: u64` and 17 more bytes. It reads the two counts as one u32,
    /// which is the tail count while no carry record holds an entry.
    pub(super) fn tails(image: &[u8], block: usize) -> Vec<(u128, u8, u64)> {
        let body = block * BLOCK + HEAD;
        let count = usize::try_from(word(image, body)).expect("invariant: a u32 fits");
        (0..count)
            .map(|number| {
                let tail = &image[body + 4 + number * 42..];
                let index =
                    u128::from_le_bytes(tail[..16].try_into().expect("16 bytes"));
                let seq = u64::from_le_bytes(tail[17..25].try_into().expect("8 bytes"));
                (index, tail[16], seq)
            })
            .collect()
    }

    /// The chain value the record leaves: a restart record's body, or its CRC.
    pub(super) fn chain(image: &[u8], block: usize) -> u32 {
        let at = if kind(image, block) == RESTART {
            HEAD
        } else {
            CRC_AT
        };
        word(image, block * BLOCK + at)
    }

    /// The CRC of the record at `block` when it continues from `chain`: over `len`,
    /// `kind`, and the body, cut at the end of the file.
    fn crc(image: &[u8], block: usize, chain: u32) -> u32 {
        let record = &image[block * BLOCK..];
        let body = len(image, block).min(record.len() - HEAD);
        let crc = crc32c::crc32c_append(chain, &record[..CRC_AT]);
        crc32c::crc32c_append(crc, &record[KIND_AT..HEAD + body])
    }

    /// Writes the CRC of the record at `block` when it continues from `chain`.
    pub(super) fn seal(image: &mut [u8], block: usize, chain: u32) {
        let crc = crc(image, block, chain);
        let at = block * BLOCK + CRC_AT;
        image[at..at + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Whether the record at `block` has a kind and its CRC continues from `chain`.
    pub(super) fn sealed(image: &[u8], block: usize, chain: u32) -> bool {
        matches!(kind(image, block), DATA | WRAP | RESTART | CARRY)
            && crc(image, block, chain) == word(image, block * BLOCK + CRC_AT)
    }
}

/// What a write of the ring must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    Fits,
    /// It must give `Full` with these bytes.
    Full {
        needed: u64,
        free: u64,
    },
    /// An open must give `Invalid`: a record it walks passes the last offset.
    Past,
    /// The target does not follow the ring after the edits.
    Unknown,
}

impl Fit {
    /// Panics when a refusal for no room is not what the fit says.
    fn refused(self, needed: u64, free: u64) {
        assert!(
            matches!(self, Self::Unknown) || self == Self::Full { needed, free },
            "the ring had no room for {needed} bytes with {free} free, against {self:?}"
        );
    }
}

/// The bytes of the largest record.
const WINDOW: u128 = (RECORD_BLOCKS * BLOCK) as u128;
/// The room that a data record leaves free.
const FREE_DATA: u128 = 3 * BLOCK as u128;
/// The room that a carry record leaves free.
const FREE_CARRY: u128 = BLOCK as u128;
/// The bytes from the low mark to the high mark of a trim on a large ring.
const STEP: u128 = 100 * BLOCK as u128;
/// The items after the reach that a carry walks to fill its record, for each tail
/// that the record may hold.
const FILL: usize = 4;
/// The bytes of one tail of a carry body, after its 4 bytes of count.
const TAIL_LEN: usize = 42;
const TAILS_MAX: usize = 1023;
/// The paths of the build, in index then path order.
const PATH_COUNT: usize = INDEXES * PATHS.len();

/// The tails that a carry body of `len` bytes holds, at most [`TAILS_MAX`].
fn tails_in(len: usize) -> usize {
    (len.saturating_sub(4) / TAIL_LEN).min(TAILS_MAX)
}

/// The place of `path` of the index `index` among the paths of the build.
fn path_at(index: usize, path: frame::Path) -> usize {
    index * PATHS.len() + usize::from(path == frame::Path::Backfill)
}

/// What a record of the ring holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// The path of each entry, and whether the entry has samples.
    Data(Vec<(usize, bool)>),
    /// Each path it carries, and whether the carried tail has a stamp.
    Carry(Vec<(usize, bool)>),
    Restart,
    /// It skips the rest of the area.
    Wrap,
}

impl Kind {
    fn byte(&self) -> u8 {
        match self {
            Self::Data(_) => record::DATA,
            Self::Carry(_) => record::CARRY,
            Self::Restart => record::RESTART,
            Self::Wrap => record::WRAP,
        }
    }
}

/// A record of the ring, from offset `start` to `end`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    start: u128,
    end: u128,
    kind: Kind,
}

/// What the trim and the carry read of a path.
#[derive(Clone, Copy, Debug, Default)]
struct Log {
    /// The record that no trim may pass.
    holder: Option<u128>,
    stamped: bool,
}

/// The ring as the target follows it, as `Writer` and `Logs` of `buffer` keep it.
#[derive(Clone, Debug)]
struct Ring {
    tail: u128,
    /// The end of the last data or carry record, or the tail: where an open puts
    /// its restart record.
    data: u128,
    /// The end of the last record: where the next record goes.
    end: u128,
    /// The records from the tail.
    records: VecDeque<Record>,
    /// The kind and blocks of the record that starts at each block of the area, as
    /// last written. A move of the tail can reach a record that a trim freed.
    disk: Vec<Option<(Kind, usize)>>,
    logs: [Log; PATH_COUNT],
    /// The number, record offset, and path of each item: the paths of each record,
    /// in ring order.
    items: VecDeque<(u64, u128, usize)>,
    /// The number of the next item.
    added: u64,
    /// The number of the first item that the next carry walks.
    scanned: u64,
}

impl Ring {
    /// A ring with its tail and records, and no state that an open recovers.
    fn walked(tail: u128, records: VecDeque<Record>, built: &Self) -> Self {
        let data = records
            .iter()
            .rev()
            .find(|record| matches!(record.kind, Kind::Data(_) | Kind::Carry(_)))
            .map_or(tail, |record| record.end);
        Self {
            tail,
            data,
            end: records.back().map_or(tail, |record| record.end),
            records,
            disk: built.disk.clone(),
            logs: [Log::default(); PATH_COUNT],
            items: VecDeque::new(),
            added: 0,
            scanned: 0,
        }
    }

    /// The bytes a record may take at `at` and leave `spare` bytes free, as
    /// `Writer::free` of `buffer` gives them.
    fn free(&self, at: u128, spare: u128) -> u128 {
        (u128::from(AREA) - (at - self.tail))
            .min(OFFSET_LAST - at)
            .saturating_sub(spare)
    }

    /// The bytes that a record of `size` bytes at `at` skips at the end of the area,
    /// when it leaves `spare` bytes free, as `Writer::cost` of `buffer` gives them.
    fn cost(
        &self,
        at: u128,
        size: u128,
        spare: u128,
    ) -> std::result::Result<u128, Fit> {
        let area = u128::from(AREA);
        let rest = area - at % area;
        let skipped = if size > rest { rest } else { 0 };
        let free = self.free(at, spare);
        let needed = skipped + size;
        let bytes = |bytes| u64::try_from(bytes).expect("invariant: under an area");
        if needed > free {
            return Err(Fit::Full {
                needed: bytes(needed),
                free: bytes(free),
            });
        }
        Ok(skipped)
    }

    /// Plans a record of `size` bytes at `head` into `planned`, after a wrap record
    /// when it skips. Gives its start. Panics when it does not fit.
    fn plan(
        &self,
        planned: &mut Vec<Record>,
        head: u128,
        size: u128,
        spare: u128,
    ) -> u128 {
        let skipped = self
            .cost(head, size, spare)
            .expect("invariant: the writer checked the room");
        if skipped > 0 {
            planned.push(Record {
                start: head,
                end: head + skipped,
                kind: Kind::Wrap,
            });
        }
        head + skipped
    }

    /// The low and high marks of a trim with the head at `head` and `queued` bytes
    /// not synced, with the saturation of `u64`.
    fn marks(head: u128, queued: u128) -> (u128, u128) {
        let room = queued.max(WINDOW) * 3;
        let lead = u128::from(AREA).saturating_sub(FREE_DATA + room);
        let step = STEP.min(lead / 2);
        let high = (head + step).min(OFFSET_LAST).saturating_sub(lead);
        (head.saturating_sub(lead), high)
    }

    /// The tail of the trim with the head at `head` and `queued` bytes not synced:
    /// the first record end at or past the high mark, but none past the oldest
    /// holder. `None` when the tail is at or past the low mark.
    fn trimmed(&self, head: u128, queued: u128) -> Option<u128> {
        let (low, high) = Self::marks(head, queued);
        if low <= self.tail {
            return None;
        }
        let ends: Vec<u128> = self.records.iter().map(|record| record.end).collect();
        let kept = self.logs.iter().filter_map(|log| log.holder).min();
        let enough = ends.partition_point(|&end| end < high);
        let free = ends.partition_point(|&end| kept.is_none_or(|kept| end <= kept));
        let last = free.checked_sub(1)?;
        Some(ends[enough.min(last)])
    }

    /// The most tails of a carry record at `head`, as `Writer::tails` of `buffer`.
    fn tails(&self, head: u128) -> usize {
        let free = self.free(head, FREE_CARRY);
        let rest = u128::from(AREA) - head % u128::from(AREA);
        let size = free.min(rest).max(free.saturating_sub(rest));
        let block = BLOCK as u128;
        let size = (size / block * block).saturating_sub(record::HEAD as u128);
        let size = usize::try_from(size).expect("invariant: under an area");
        tails_in(size).min(tails_in(BODY_MAX))
    }

    /// Hides the items of the records before `tail`.
    fn hide(&mut self, tail: u128) {
        while self
            .items
            .front()
            .is_some_and(|&(_, offset, _)| offset < tail)
        {
            self.items.pop_front();
        }
    }

    /// Frees the records before `tail`.
    fn release(&mut self, tail: u128) {
        self.tail = tail;
        self.data = self.data.max(tail);
        while self
            .records
            .front()
            .is_some_and(|record| record.end <= tail)
        {
            self.records.pop_front();
        }
    }

    fn item(&mut self, offset: u128, path: usize) {
        self.items.push_back((self.added, offset, path));
        self.added += 1;
    }

    /// Syncs the entries of the data record at `offset`, as `Logs::sync` of
    /// `buffer`: an entry with samples, or one on a path with no stamp, makes the
    /// record the holder.
    fn sync(&mut self, offset: u128, entries: &[(usize, bool)]) {
        let mut met = [false; PATH_COUNT];
        for &(path, samples) in entries {
            let log = &mut self.logs[path];
            if samples || !log.stamped {
                log.holder = Some(offset);
            }
            log.stamped |= samples;
            if !met[path] {
                met[path] = true;
                self.item(offset, path);
            }
        }
    }

    /// The paths that a carry record holds, as `Logs::carry` of `buffer` gives
    /// them: each path whose holder is before `reach`, then the holders of the next
    /// items after it, at most `limit`.
    fn carried(&mut self, reach: u128, limit: usize) -> Vec<usize> {
        let mut carried = Vec::new();
        let mut walk = limit.saturating_mul(FILL);
        let from = self.scanned;
        for at in 0..self.items.len() {
            let (number, offset, path) = self.items[at];
            if number < from {
                continue;
            }
            if offset >= reach {
                if carried.is_empty() || walk == 0 {
                    self.scanned = number;
                    return carried;
                }
                walk -= 1;
            }
            if self.logs[path].holder != Some(offset) {
                continue;
            }
            if carried.len() == limit {
                self.scanned = number;
                return carried;
            }
            carried.push(path);
        }
        self.scanned = self.added;
        carried
    }

    /// Writes `record` after the last one.
    fn put(&mut self, record: Record) {
        let block = BLOCK as u128;
        let at = usize::try_from(record.start % u128::from(AREA) / block)
            .expect("invariant: a block of the area");
        let blocks = usize::try_from((record.end - record.start) / block)
            .expect("invariant: a record fits the area");
        if record.kind != Kind::Wrap {
            // A wrap record writes its first block only.
            self.disk[at..at + blocks].fill(None);
        }
        self.disk[at] = Some((record.kind.clone(), blocks));
        self.records.push_back(record);
    }

    /// Writes a restart record at the end.
    fn restart(&mut self) {
        let start = self.end;
        self.end += BLOCK as u128;
        self.put(Record {
            start,
            end: self.end,
            kind: Kind::Restart,
        });
    }

    /// A commit of the data record of `entries` in `blocks` blocks, with its trim
    /// and its carry record. Gives the start of the data record.
    fn commit(&mut self, entries: Vec<(usize, bool)>, blocks: usize) -> u128 {
        let mut planned = Vec::new();
        let queued = (blocks * BLOCK) as u128;
        let data = self.plan(&mut planned, self.end, queued, FREE_DATA);
        let mut head = data + queued;
        planned.push(Record {
            start: data,
            end: head,
            kind: Kind::Data(entries),
        });
        let trim = self.trimmed(head, queued);
        if let Some(tail) = trim {
            self.hide(tail);
        }
        let (_, reach) = Self::marks((head + queued).min(OFFSET_LAST), queued);
        let carried = self.carried(reach, self.tails(head));
        let tails: Vec<(usize, bool)> = carried
            .iter()
            .map(|&path| (path, self.logs[path].stamped))
            .collect();
        for record in planned {
            if let Kind::Data(entries) = &record.kind {
                self.sync(record.start, entries);
            }
            self.data = record.end;
            self.put(record);
        }
        if !carried.is_empty() {
            let len = record::HEAD + 4 + TAIL_LEN * carried.len();
            let size = len.next_multiple_of(BLOCK) as u128;
            let mut planned = Vec::new();
            let start = self.plan(&mut planned, head, size, FREE_CARRY);
            head = start + size;
            planned.push(Record {
                start,
                end: head,
                kind: Kind::Carry(tails),
            });
            for path in carried {
                self.logs[path].holder = Some(start);
                self.item(start, path);
            }
            for record in planned {
                self.put(record);
            }
            self.data = head;
        }
        self.end = head;
        if let Some(tail) = trim {
            self.release(tail);
        }
        data
    }

    /// The state that an open recovers from the walk of the records up to the end
    /// of the last data or carry record. The records after it go.
    fn recover(&mut self) {
        while self
            .records
            .back()
            .is_some_and(|record| record.start >= self.data)
        {
            self.records.pop_back();
        }
        self.end = self.data;
        self.logs = [Log::default(); PATH_COUNT];
        self.items.clear();
        (self.added, self.scanned) = (0, 0);
        for at in 0..self.records.len() {
            let Record { start, kind, .. } = self.records[at].clone();
            match kind {
                Kind::Data(entries) => self.sync(start, &entries),
                Kind::Carry(tails) => {
                    for (path, stamped) in tails {
                        let log = &mut self.logs[path];
                        log.holder = Some(start);
                        log.stamped |= stamped;
                        self.item(start, path);
                    }
                }
                Kind::Restart | Kind::Wrap => {}
            }
        }
    }

    /// An open: it recovers, trims as a commit with nothing queued does, and puts
    /// its restart record of one block at the end of the last data or carry record,
    /// after the trim when it needs its room.
    fn open(&mut self) -> Fit {
        if self.end > OFFSET_LAST {
            return Fit::Past;
        }
        self.recover();
        let trim = self.trimmed(self.end, 0);
        let placed = self.cost(self.end, BLOCK as u128, 0);
        match (placed, trim) {
            (Err(full), None) => return full,
            (Ok(_), _) => self.restart(),
            (Err(_), Some(_)) => {}
        }
        if let Some(tail) = trim {
            self.hide(tail);
            self.release(tail);
        }
        if placed.is_err() {
            if let Err(full) = self.cost(self.end, BLOCK as u128, 0) {
                return full;
            }
            self.restart();
        }
        Fit::Fits
    }

    /// The entries of each path of the build in the data records before `tail`.
    fn passed(&self, tail: u128) -> [usize; PATH_COUNT] {
        let mut passed = [0; PATH_COUNT];
        for record in self.records.iter().take_while(|record| record.start < tail) {
            if let Kind::Data(entries) = &record.kind {
                for &(path, _) in entries {
                    passed[path] += 1;
                }
            }
        }
        passed
    }
}

/// The ring as the target follows it, or `None` after edits it does not follow.
#[derive(Clone, Debug)]
struct Room(Option<Ring>);

impl Room {
    /// A new ring: its restart record at offset 0.
    fn new() -> Self {
        let mut ring = Ring {
            tail: 0,
            data: 0,
            end: 0,
            records: VecDeque::new(),
            disk: vec![None; BLOCKS],
            logs: [Log::default(); PATH_COUNT],
            items: VecDeque::new(),
            added: 0,
            scanned: 0,
        };
        ring.restart();
        Self(Some(ring))
    }

    /// The room an open finds in `image` after `edits`, from the records that it
    /// takes ([`taken`]). Unknown after an edit other than `MoveTail`, whose records
    /// the open can read in other ways.
    ///
    /// # Panics
    ///
    /// When it takes a record that this room did not write where it starts.
    fn walked(&self, image: &[u8], edits: &[Edit]) -> Self {
        let moved = edits
            .iter()
            .all(|edit| matches!(edit, Edit::MoveTail { .. }));
        let (Some(built), Some(start), true) = (&self.0, header::read(image), moved)
        else {
            return Self(None);
        };
        let tail = u128::from(header::tail(image, start));
        let mut records = VecDeque::new();
        let mut end = tail;
        for (block, blocks) in taken(image) {
            let written = built.disk[block - AREA_START].clone();
            let kind = match written {
                Some((kind, size)) if size == blocks => kind,
                _ => panic!("the open takes a record at {block} that no build wrote"),
            };
            assert_eq!(
                kind.byte(),
                record::kind(image, block),
                "the kind at {block}"
            );
            let start = end;
            end += (blocks * BLOCK) as u128;
            records.push_back(Record { start, end, kind });
        }
        Self(Some(Ring::walked(tail, records, built)))
    }

    /// The offsets and the records of the ring, which an open recovers from.
    fn shape(&self) -> Option<(u128, u128, u128, &VecDeque<Record>)> {
        let ring = self.0.as_ref()?;
        Some((ring.tail, ring.data, ring.end, &ring.records))
    }

    fn tail(&self) -> Option<u128> {
        self.0.as_ref().map(|ring| ring.tail)
    }

    fn open(&mut self) -> Fit {
        self.0.as_mut().map_or(Fit::Unknown, Ring::open)
    }

    /// Whether an append of a record of `blocks` blocks fits after the last record.
    /// It leaves three blocks free: for the restart record of the next open, the
    /// carry record of the commit after it, and the restart record of a later open.
    fn take(&self, blocks: usize) -> Fit {
        let Some(ring) = &self.0 else {
            return Fit::Unknown;
        };
        let fit = ring.cost(ring.end, (blocks * BLOCK) as u128, FREE_DATA);
        fit.map_or_else(|full| full, |_| Fit::Fits)
    }

    /// The commit of a record of `blocks` blocks that [`take`](Self::take) took,
    /// with the path of each entry and whether it has samples. Gives the start of
    /// the record.
    fn commit(&mut self, entries: Vec<(usize, bool)>, blocks: usize) -> Option<u128> {
        Some(self.0.as_mut()?.commit(entries, blocks))
    }

    /// The entries of each path of the build in the records before `tail`.
    fn passed(&self, tail: u128) -> Option<[usize; PATH_COUNT]> {
        Some(self.0.as_ref()?.passed(tail))
    }
}

/// Whether an append took its batch. Panics when that is not what `fit` says.
fn took(taken: std::result::Result<(), Rejected>, fit: Fit) -> bool {
    match (taken, fit) {
        (Ok(()), Fit::Fits | Fit::Unknown) => true,
        (Err(Rejected::Full { needed, free }), _) => {
            fit.refused(needed, free);
            false
        }
        (taken, fit) => panic!("an append gave {taken:?} where the ring gives {fit:?}"),
    }
}

/// The time an entry with `tag` is stored.
fn stored_at(tag: u8) -> Stamp {
    Stamp::from_nanos(i64::from(tag))
}

/// One entry of a path, as appended or as a read gave it, with its bytes off the
/// pool, so the entries of one read do not take the blocks of the next.
#[derive(Debug, PartialEq, Eq)]
struct Given {
    first: u64,
    samples: Option<Samples>,
    stored_at: Stamp,
    tag: u8,
    bytes: Vec<u8>,
}

impl Given {
    /// How many samples: 0 for an entry with no samples.
    fn len(&self) -> u32 {
        self.samples.map_or(0, |samples| samples.len.get())
    }
}

impl From<&Stored> for Given {
    fn from(entry: &Stored) -> Self {
        let Stored {
            first,
            samples,
            stored_at,
            tag,
            bytes,
        } = entry;
        Self {
            first: *first,
            samples: *samples,
            stored_at: *stored_at,
            tag: *tag,
            bytes: bytes.to_vec(),
        }
    }
}

impl From<&Entry> for Given {
    fn from(entry: &Entry) -> Self {
        Self {
            first: entry.first,
            samples: entry.samples,
            stored_at: entry.stored_at,
            tag: entry.tag,
            bytes: entry
                .parts
                .clone()
                .into_iter()
                .flat_map(|b| b.to_vec())
                .collect(),
        }
    }
}

/// One entry the build phase appends.
#[derive(Debug)]
struct Append {
    index: usize,
    path: frame::Path,
    /// Seqs the entry leaves out before its first.
    skip: u64,
    /// Samples in the entry: 0 for a caller record.
    len: u32,
    /// The bytes of each part: none, one, or two.
    parts: Vec<usize>,
}

impl Append {
    /// The entry of this append on `slot`, with `tag`, from the next seq of its path
    /// in `firsts`, which it moves past the entry.
    fn entry(
        &self,
        shard: &Shard,
        slot: Slot,
        tag: u8,
        firsts: &mut [[u64; 2]],
    ) -> Entry {
        let path = usize::from(self.path == frame::Path::Backfill);
        let first = firsts[self.index][path] + self.skip;
        firsts[self.index][path] = first + u64::from(self.len);
        let samples = NonZeroU32::new(self.len).map(|len| Samples {
            len,
            last: Some(Stamp::from_nanos(i64::try_from(first).expect("small"))),
        });
        let large = self.parts.iter().sum::<usize>() > Layout::ENTRY_MAX_MIN;
        let mut blocks = self.parts.iter().map(|len| shard.block(*len, tag));
        let parts = match (blocks.next(), blocks.next()) {
            (Some(one), Some(two)) => Parts::from([one, two]),
            (one, _) => Parts::from(one),
        };
        Entry {
            index: key(self.index),
            slot,
            path: self.path,
            first,
            samples,
            stored_at: stored_at(tag),
            tag: if large { 0 } else { tag },
            parts,
        }
    }

    /// Small entry `number` of a batch, with at most 72 bytes. The entries differ:
    /// samples or a caller record, no part or two, a skip ahead or none.
    fn small(number: usize) -> Self {
        Self {
            index: number % INDEXES,
            path: PATHS[number / INDEXES % PATHS.len()],
            skip: if number % 5 == 2 { 2 } else { 0 },
            len: u32::from(number % 4 != 3),
            parts: if number % 7 == 1 {
                vec![number % 61, number % 13]
            } else {
                Vec::new()
            },
        }
    }
}

/// One change to the file bytes.
#[derive(Debug)]
enum Edit {
    Put { at: usize, bytes: Vec<u8> },
    Zero { at: usize, len: usize },
    SealHeader { second: bool },
    SealRecord { block: usize },
    MoveTail { block: usize, lap: u64 },
}

#[derive(Debug)]
struct Input {
    /// The replay value of the run, from the input bytes. Bytes on a disk cannot
    /// hold a chain that a later open draws, so an edit must not either.
    replay: u64,
    batches: Vec<Vec<Append>>,
    edits: Vec<Edit>,
    /// Bytes of each entry the check commits.
    check_part: usize,
}

/// FNV-1a of 64 bits. Its definition is fixed, so a stored input keeps its run.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let replay = fnv(u.peek_bytes(u.len()).expect("the input has its length"));
        let mut batches = Vec::new();
        for _ in 0..u.int_in_range(0..=4)? {
            let mut appends = Vec::new();
            for _ in 0..u.int_in_range(1..=4)? {
                appends.push(Append {
                    index: u.int_in_range(0..=INDEXES - 1)?,
                    path: PATHS[usize::from(u.int_in_range(0..=1u8)?)],
                    skip: 0,
                    len: u.int_in_range(1..=16)?,
                    parts: vec![u.int_in_range(0..=PART_MAX)?],
                });
            }
            batches.push(appends);
        }
        let blocks = AREA_START..=AREA_START + BLOCKS - 1;
        let mut edits = Vec::new();
        for _ in 0..u.int_in_range(0..=16)? {
            edits.push(match u.int_in_range(0..=4u8)? {
                0 => Edit::Put {
                    at: u.int_in_range(0..=FILE_LEN - 1)?,
                    bytes: u.arbitrary::<[u8; 8]>()?[..u.int_in_range(1..=8)?].to_vec(),
                },
                1 => Edit::Zero {
                    at: u.int_in_range(0..=FILE_LEN - 1)?,
                    len: u.int_in_range(1..=FILE_LEN)?,
                },
                2 => Edit::SealHeader {
                    second: u.arbitrary()?,
                },
                3 => Edit::SealRecord {
                    block: u.int_in_range(blocks.clone())?,
                },
                _ => {
                    let block = u.int_in_range(blocks.clone())?;
                    Edit::MoveTail {
                        block,
                        lap: match u.arbitrary::<u8>()? {
                            low @ ..128 => u64::from(low),
                            high => lap_last(block) - u64::from(255 - high),
                        },
                    }
                }
            });
        }
        // Last, so an input that ends before these has no small entry and the least
        // commit.
        for batch in &mut batches {
            batch.extend((0..u.int_in_range(0..=255)?).map(Append::small));
        }
        Ok(Self {
            replay,
            batches,
            edits,
            check_part: u.int_in_range(CHECK_PART..=CHECK_PART_MAX)?,
        })
    }
}

fn apply(image: &mut [u8], edit: &Edit) {
    match edit {
        Edit::Put { at, bytes } => {
            let len = bytes.len().min(image.len() - at);
            image[*at..at + len].copy_from_slice(&bytes[..len]);
        }
        Edit::Zero { at, len } => {
            let len = (*len).min(image.len() - at);
            image[*at..at + len].fill(0);
        }
        Edit::SealHeader { second } => {
            header::seal(image, if *second { BLOCK } else { 0 })
        }
        Edit::SealRecord { block } => {
            let chain = chain_before(image, *block);
            record::seal(image, *block, chain);
        }
        Edit::MoveTail { block, lap } => {
            let chain = chain_before(image, *block);
            let start = header::chosen(image);
            header::put_tail(image, start, offset(*block, *lap), chain);
        }
    }
}

/// The chain value a record at `block` must continue from: the one that the
/// records from the tail in the header block that the open reads leave there. When
/// no record starts at `block`, the value where the way ends.
fn chain_before(image: &[u8], block: usize) -> u32 {
    let way = walk(image);
    let end = way.last().expect("invariant: the way starts at the tail");
    way.iter()
        .find(|&&(start, _)| start == block)
        .unwrap_or(end)
        .1
}

/// The way of the records from the tail in the header block that the open reads:
/// the block each starts at with the chain value it continues from, then the block
/// and the chain value after the last. It ends where it comes back to the tail or
/// passes the end of the area. The CRCs on the way are not checked.
fn walk(image: &[u8]) -> Vec<(usize, u32)> {
    let start = header::chosen(image);
    let (tail, _) = place(header::tail(image, start));
    let mut way = vec![(tail, header::chain(image, start))];
    let mut block = tail;
    for _ in 0..BLOCKS {
        let next = match record::kind(image, block) {
            record::WRAP => AREA_START,
            _ => block + (record::HEAD + record::len(image, block)).div_ceil(BLOCK),
        };
        let next = if next == AREA_START + BLOCKS {
            AREA_START
        } else {
            next
        };
        way.push((next, record::chain(image, block)));
        if next == tail || next > AREA_START + BLOCKS {
            break;
        }
        block = next;
    }
    way
}

/// The records that the open takes on the way from the tail in the header block
/// that it reads: the block each starts at, and the blocks to the next. It ends
/// before the first record that is not sealed, whose body is over the header's
/// `body_max`, or that passes the end of the area, the tail one lap later, or the
/// largest record of the header's `body_max`.
fn taken(image: &[u8]) -> Vec<(usize, usize)> {
    let body_max = header::body_max(image, header::chosen(image));
    let largest = (record::HEAD + body_max).div_ceil(BLOCK);
    let mut lap = BLOCKS;
    let mut records = Vec::new();
    for step in walk(image).windows(2) {
        let [(block, chain), (next, _)] = *step else {
            unreachable!("a window of two")
        };
        let size = (record::HEAD + record::len(image, block)).div_ceil(BLOCK);
        let bound = largest.min(AREA_START + BLOCKS - block).min(lap);
        if size > bound
            || record::len(image, block) > body_max
            || !record::sealed(image, block, chain)
        {
            break;
        }
        let blocks = if next > block {
            next - block
        } else {
            next + BLOCKS - block
        };
        lap = lap.saturating_sub(blocks);
        records.push((block, blocks));
    }
    records
}

fn key(index: usize) -> channel::Key {
    channel::Key::from_u128(index as u128)
}

/// The shard of the run.
struct Shard {
    node: sim::node::Node,
    tasks: Tasks,
    pool: Rc<Pool>,
}

impl Shard {
    /// Opens the ring. Gives the slot of each index of the build, then each other
    /// slot up to a new data slot of `SPARE`, which comes after each slot the open
    /// made: an edit can change the index of an entry.
    async fn open(&self) -> std::result::Result<(Buffer, Vec<Slot>), Error> {
        let mut table = Slots::new();
        let config = Config {
            files: self.node.files(),
            dir: PathBuf::from(DIR),
            pool: Rc::clone(&self.pool),
            clock: self.node.clock(),
            tasks: self.tasks.clone(),
            entropy: self.node.entropy(),
            layout: Layout::new(AREA, BODY_MAX)
                .expect("invariant: the sizes make a ring"),
            commit: COMMIT,
        };
        let buffer = Buffer::open(config, &mut table).await?;
        let mut slots: Vec<Slot> =
            (0..INDEXES).map(|index| table.index(key(index))).collect();
        let edited: Vec<Slot> = (0..=table.data(SPARE, Type::Scalar(Scalar::U8)).get())
            .map(Slot::new)
            .filter(|slot| !slots.contains(slot))
            .collect();
        slots.extend(edited);
        Ok((buffer, slots))
    }

    /// A block of `len` bytes of `tag`.
    fn block(&self, len: usize, tag: u8) -> Block {
        let mut block = self.pool.alloc(len).expect("the pool has a block");
        block.fill(tag);
        block.freeze()
    }
}

/// What the build left.
struct Built {
    /// The durable tail of each path, in slot then path order.
    tails: Vec<Tail>,
    /// The entries of each path, in the same order, each with the start of its
    /// record.
    written: Vec<Vec<(u128, Given)>>,
    room: Room,
}

/// Writes the batches to a new ring. An append refuses a batch exactly when `limit`
/// does, and the build goes on after it. The ring takes each other batch: the area
/// holds one largest record of each commit.
async fn build(shard: &Shard, input: &Input) -> Built {
    let (buffer, slots) = shard.open().await.expect("a new ring opens");
    let mut room = Room::new();
    let mut next = vec![[0u64; 2]; INDEXES];
    let mut tags = (BUILD_TAG..CHECK_TAG).cycle();
    let mut written: Vec<[Vec<(u128, Given)>; 2]> =
        iter::repeat_with(Default::default).take(INDEXES).collect();
    for batch in &input.batches {
        let mut firsts = next.clone();
        let mut appended = Vec::new();
        let mut entries = Vec::new();
        let mut kinds = Vec::new();
        let (mut parts, mut bytes) = (0, 0);
        for (append, tag) in batch.iter().zip(&mut tags) {
            let path = usize::from(append.path == frame::Path::Backfill);
            let entry = append.entry(shard, slots[append.index], tag, &mut firsts);
            parts += append.parts.len();
            let given = Given::from(&entry);
            bytes += given.bytes.len();
            appended.push((append.index, path, given));
            kinds.push((path_at(append.index, append.path), append.len > 0));
            entries.push(entry);
        }
        let fits = limit(BODY_MAX, entries.len(), parts, bytes);
        let checked = buffer.layout().check(entries.len(), parts, bytes);
        assert_eq!(checked, fits, "the layout check is not the record limits");
        let blocks = (record::HEAD + body(entries.len(), bytes)).div_ceil(BLOCK);
        let taken = buffer.append(entries);
        if let Err(limit) = fits {
            assert_eq!(
                taken,
                Err(Rejected::Large(limit)),
                "an append refused a batch other than by the record limits"
            );
            continue;
        }
        // The area holds one largest record of each commit (`BLOCKS`).
        assert!(
            took(taken, room.take(blocks)),
            "the ring refused the one record of a commit"
        );
        next = firsts;
        let start = room
            .commit(kinds, blocks)
            .expect("the build knows its ring");
        buffer.committed().await.expect("the batch commits");
        for (index, path, entry) in appended {
            written[index][path].push((start, entry));
        }
    }
    let tails = tails(&buffer, &slots);
    let ended = buffer.ended();
    drop(buffer);
    ended.await.expect("the build ends");
    Built {
        tails,
        written: written.into_iter().flatten().collect(),
        room,
    }
}

/// The first limit of one record that a batch of `entries` entries, `parts` parts,
/// and `bytes` bytes is over: 1023 entries, 1023 parts, and a body of `body_max`.
fn limit(
    body_max: usize,
    entries: usize,
    parts: usize,
    bytes: usize,
) -> std::result::Result<(), Limit> {
    const COUNT_MAX: usize = 1023;
    let len = body(entries, bytes);
    if entries > COUNT_MAX {
        Err(Limit::Entries { count: entries })
    } else if parts > COUNT_MAX {
        Err(Limit::Parts { count: parts })
    } else if len > body_max {
        Err(Limit::Body { len, max: body_max })
    } else {
        Ok(())
    }
}

/// Every entry of each path from the start, in slot then path order.
async fn read_paths(buffer: &Buffer, slots: &[Slot], carried: bool) -> Vec<Vec<Given>> {
    let mut paths = Vec::new();
    for slot in slots {
        for path in PATHS {
            let whole = read_path(buffer, *slot, path, usize::MAX, carried).await;
            for budget in [1, BUDGET] {
                let stepped = read_path(buffer, *slot, path, budget, carried).await;
                assert_eq!(stepped, whole, "reads of {budget} gave other entries");
            }
            let mut at = 0;
            for entry in &whole {
                if entry.first - at > 1 {
                    read_from(buffer, *slot, path, at + 1, entry).await;
                }
                if entry.len() > 1 {
                    read_from(buffer, *slot, path, entry.first + 1, entry).await;
                }
                at = entry.first + u64::from(entry.len());
            }
            paths.push(whole);
        }
    }
    paths
}

/// Reads from seq `from`, which is in the gap before `entry` or inside it. The read
/// must report the rest of the gap and give `entry` first, whole.
async fn read_from(
    buffer: &Buffer,
    slot: Slot,
    path: frame::Path,
    from: u64,
    entry: &Given,
) {
    let read = buffer
        .reader()
        .read(slot, path, Mark::at(from), 1)
        .await
        .expect("a read of an open ring");
    let gap = (entry.first > from).then_some(from..entry.first);
    assert_eq!(read.gap, gap, "the gap of a read from seq {from}");
    let first = read.entries.first().map(Given::from);
    assert_eq!(first.as_ref(), Some(entry), "a read from seq {from}");
}

/// Every entry of `path` from the start, in reads of `budget`.
///
/// # Panics
///
/// When a read breaks the doc of `Reader::read`.
async fn read_path(
    buffer: &Buffer,
    slot: Slot,
    path: frame::Path,
    budget: usize,
    carried: bool,
) -> Vec<Given> {
    let tail = buffer.durable(slot, path);
    let mut given: Vec<Given> = Vec::new();
    let mut from = Mark::at(0);
    let mut under = false;
    loop {
        let read = buffer
            .reader()
            .read(slot, path, from, budget)
            .await
            .expect("a read of an open ring");
        assert!(read.next.seq <= tail.seq, "a read went past the tail");
        if read.entries.is_empty() {
            if let Some(gap) = read.gap {
                assert!(carried, "a gap with no entry and no carry record");
                assert_eq!(gap, from.seq..tail.seq, "a gap with no entry");
                assert_eq!(read.next.seq, tail.seq, "a gap with no entry");
                from = read.next;
                continue;
            }
            assert_eq!(read.next, from, "an empty read moved the mark");
            assert_eq!(from.seq, tail.seq, "the reads did not end at the tail");
            let stamp = given
                .iter()
                .rev()
                .find_map(|entry| entry.samples.and_then(|s| s.last));
            if stamp.is_some() || !carried {
                assert_eq!(stamp, tail.stamp, "the reads did not give the tail stamp");
            }
            return given;
        }
        assert!(
            !under || read.gap.is_some(),
            "a read stopped before its budget, a skip, or the tail"
        );
        let mut at = from.seq;
        let mut spent = 0;
        for (number, entry) in read.entries.iter().enumerate() {
            assert!(spent < budget, "entry {number} came past the budget");
            spent += block::footprint(entry.bytes.len());
            if number == 0 {
                assert!(entry.first >= at, "the first entry is below the mark");
                let gap = (entry.first > at).then_some(at..entry.first);
                assert_eq!(read.gap, gap, "the gap before the first entry");
            } else {
                assert_eq!(entry.first, at, "a gap before entry {number}");
            }
            at = entry.seqs().end;
            given.push(Given::from(entry));
        }
        assert_eq!(read.next.seq, at, "next is not after the last entry");
        assert!(
            read.next > from,
            "a read with entries did not move the mark"
        );
        under = spent < budget;
        from = read.next;
    }
}

/// The durable tail of each path, in slot then path order.
fn tails(buffer: &Buffer, slots: &[Slot]) -> Vec<Tail> {
    slots
        .iter()
        .flat_map(|slot| PATHS.map(|path| buffer.durable(*slot, path)))
        .collect()
}

/// The bytes of the ring file.
async fn image(file: &File, pool: &Pool) -> Vec<u8> {
    let mut image = Vec::with_capacity(FILE_LEN);
    for block in 0..FILE_LEN / BLOCK {
        let into = pool.alloc(BLOCK).expect("the pool has a block");
        let read = file
            .read_at((block * BLOCK) as u64, into)
            .await
            .expect("the ring reads");
        image.extend_from_slice(&read);
    }
    image
}

/// Applies the edits to the ring file. Gives the room an open then finds.
async fn edit(file: &File, pool: &Rc<Pool>, edits: &[Edit], built: &Room) -> Room {
    let mut image = image(file, pool).await;
    assert_eq!(
        built.walked(&image, &[]).shape(),
        built.shape(),
        "the records of the build do not seal as written up to its last"
    );
    let written = image.clone();
    for (block, _) in taken(&written) {
        apply(&mut image, &Edit::SealRecord { block });
        assert!(
            image == written,
            "`SealRecord` restates the record at {block}"
        );
    }
    let (block, lap) = place(header::tail(&image, header::chosen(&image)));
    apply(&mut image, &Edit::MoveTail { block, lap });
    assert!(image == written, "`MoveTail` restates the header");
    for edit in edits {
        apply(&mut image, edit);
    }
    let blocks: Vec<Block> = image
        .chunks(BLOCK)
        .map(|bytes| pool.copy(bytes).expect("the pool has a block"))
        .collect();
    file.write_at(0, &blocks).await.expect("the ring writes");
    file.sync().await.expect("the ring syncs");
    built.walked(&image, edits)
}

/// The index, the path byte, and the seq of each tail of the carry records that an
/// open takes ([`taken`]). A path that one holds may have a tail and no entry.
async fn carried(shard: &Shard) -> Vec<(u128, u8, u64)> {
    let file = shard
        .node
        .files()
        .open(Path::new(RING), Mode::Read)
        .await
        .expect("the ring is there");
    let image = image(&file, &shard.pool).await;
    taken(&image)
        .into_iter()
        .filter(|&(block, _)| record::kind(&image, block) == record::CARRY)
        .flat_map(|(block, _)| record::tails(&image, block))
        .collect()
}

/// Opens the changed ring, which must open or refuse as `room` says. With no edits,
/// it must give the tails and the sizes the build left, and the entries of the
/// records that its trim keeps. When it opens, each path must read. One commit on
/// it must then give `Rejected::Large` when the record limits in the ring header
/// refuse it, or else fit or give `Rejected::Full` as `room` says. A commit that
/// fits must survive a reopen, which must give the entries of the records that its
/// trim keeps.
///
/// The commit is one entry at each tail, of the input's `check_part` bytes. A record
/// edit can put a tail at `u64::MAX`, a precondition of `append`, so such a ring is
/// not checked.
async fn check(shard: &Shard, input: &Input, built: &Built, mut room: Room) {
    let fit = room.open();
    let (buffer, slots) = match (shard.open().await, fit) {
        (Err(Error::Pool(_) | Error::Files(_)), _) => {
            panic!("open failed outside the ring")
        }
        (Ok(opened), Fit::Fits | Fit::Unknown) => opened,
        (Err(Error::Full { needed, free }), _) => return fit.refused(needed, free),
        (Err(Error::Invalid { .. }), Fit::Past) | (Err(_), Fit::Unknown) => return,
        (opened, fit) => {
            let opened = opened.map(drop);
            panic!("an open gave {opened:?} where the ring gives {fit:?}")
        }
    };
    let read = read_paths(&buffer, &slots, !carried(shard).await.is_empty()).await;
    // An edit can change the sizes in the header, and the ring keeps them.
    let body_max = buffer.layout().body_max();
    if input.edits.is_empty() {
        check_build(&buffer, &slots, built, &room, &read);
    }
    let Some((entries, kinds, commits)) = commit_entries(shard, &buffer, &slots, input)
    else {
        return;
    };
    let before = tails(&buffer, &slots);
    let count = entries.len();
    let bytes = input.check_part * count;
    let fits = limit(body_max, count, count, bytes);
    let checked = buffer.layout().check(count, count, bytes);
    assert_eq!(checked, fits, "the layout check is not the record limits");
    let taken = buffer.append(entries);
    if let Err(limit) = fits {
        assert_eq!(
            taken,
            Err(Rejected::Large(limit)),
            "an append refused a commit other than by the record limits"
        );
        return;
    }
    let blocks = check_record(input.check_part).div_ceil(BLOCK);
    if !took(taken, room.take(blocks)) {
        return;
    }
    room.commit(kinds, blocks);
    assert_eq!(
        tails(&buffer, &slots),
        before,
        "an entry was durable before its commit"
    );
    buffer.committed().await.expect("the entries commit");
    let durable = tails(&buffer, &slots);
    let stored = read_paths(&buffer, &slots, !carried(shard).await.is_empty()).await;
    // The paths of the build come first.
    for (given, commit) in stored.iter().zip(&commits) {
        assert_eq!(given.last(), Some(commit), "a read lost the commit");
    }
    drop(buffer);
    reopen(shard, room, &durable, &stored, &commits).await;
}

/// Checks an open of the ring that the build wrote, with no edit: it keeps the sizes
/// and the tails of the build, and `read` gives each entry of the build after the
/// tail of `room`, and nothing else.
fn check_build(
    buffer: &Buffer,
    slots: &[Slot],
    built: &Built,
    room: &Room,
    read: &[Vec<Given>],
) {
    assert_eq!(
        buffer.layout().body_max(),
        BODY_MAX,
        "an open lost the sizes the build wrote"
    );
    assert_eq!(
        tails(buffer, slots),
        built.tails,
        "an open lost what the build wrote"
    );
    let tail = room
        .tail()
        .expect("the target follows a ring with no edits");
    let kept: Vec<Vec<&Given>> = built
        .written
        .iter()
        .map(|path| {
            let kept = path.iter().filter(|(start, _)| *start >= tail);
            kept.map(|(_, entry)| entry).collect()
        })
        .collect();
    let (build, rest) = read.split_at(built.written.len());
    let build: Vec<Vec<&Given>> =
        build.iter().map(|path| path.iter().collect()).collect();
    assert_eq!(build, kept, "a read lost what the build wrote");
    assert!(
        rest.iter().all(Vec::is_empty),
        "a read gave what no build wrote"
    );
}

/// The entries of the check commit, one after the tail of each path of the build,
/// with the kind that the room takes for each and what a read gives of each. `None`
/// when a tail is at the last seq.
fn commit_entries(
    shard: &Shard,
    buffer: &Buffer,
    slots: &[Slot],
    input: &Input,
) -> Option<(Vec<Entry>, Vec<(usize, bool)>, Vec<Given>)> {
    let mut entries = Vec::new();
    let mut kinds = Vec::new();
    let mut commits = Vec::new();
    for (index, slot) in slots[..INDEXES].iter().enumerate() {
        for path in PATHS {
            let tail = buffer.tail(*slot, path);
            if tail.seq == u64::MAX {
                return None;
            }
            let tag = CHECK_TAG + u8::try_from(commits.len()).expect("few paths");
            let entry = Entry {
                index: key(index),
                slot: *slot,
                path,
                first: tail.seq,
                samples: Some(Samples {
                    len: NonZeroU32::MIN,
                    last: Some(CHECK_LAST),
                }),
                stored_at: stored_at(tag),
                tag,
                parts: Parts::from(shard.block(input.check_part, tag)),
            };
            commits.push(Given::from(&entry));
            kinds.push((path_at(index, path), true));
            entries.push(entry);
        }
    }
    Some((entries, kinds, commits))
}

/// Reopens the ring after the check commit. The reopen recovers the tails `durable`
/// that the commit gave, and each path gives what `stored` gave past the trim of
/// the reopen, up to its entry of `commits`. The trim frees that entry only when a
/// carry record holds the tail after it.
async fn reopen(
    shard: &Shard,
    mut room: Room,
    durable: &[Tail],
    stored: &[Vec<Given>],
    commits: &[Given],
) {
    let before = room.clone();
    let fit = room.open();
    let (reopened, slots) = match (shard.open().await, fit) {
        (Ok(opened), Fit::Fits | Fit::Unknown) => opened,
        (Err(Error::Full { needed, free }), _) => return fit.refused(needed, free),
        (opened, fit) => {
            let opened = opened.map(drop);
            panic!(
                "a reopen after a commit gave {opened:?} where the ring gives {fit:?}"
            )
        }
    };
    let recovered: Vec<Tail> = slots
        .iter()
        .flat_map(|slot| PATHS.map(|path| reopened.tail(*slot, path)))
        .collect();
    assert_eq!(recovered, durable, "a reopen lost what committed reported");
    let carry = carried(shard).await;
    let reread = read_paths(&reopened, &slots, !carry.is_empty()).await;
    assert_eq!(reread.len(), stored.len(), "a reopen changed the paths");
    let passed = room.tail().and_then(|tail| before.passed(tail));
    for (path, (given, kept)) in reread.iter().zip(stored).enumerate() {
        let Some(passed) = passed else {
            assert!(
                kept.ends_with(given),
                "a reopen changed what path {path} gives"
            );
            let Some(commit) = commits.get(path) else {
                continue;
            };
            let end = commit.first + u64::from(commit.len());
            let index = u128::try_from(path / PATHS.len()).expect("few paths");
            let byte = u8::try_from(path % PATHS.len()).expect("two paths");
            if !(given.is_empty() && carry.contains(&(index, byte, end))) {
                assert_eq!(given.last(), Some(commit), "a reopen lost the commit");
            }
            continue;
        };
        let from = passed.get(path).copied().unwrap_or(0);
        assert_eq!(
            Some(&given[..]),
            kept.get(from..),
            "a reopen changed what path {path} gives past its trim"
        );
    }
}

fuzz_target!(|input: Input| {
    let mut sim = sim::Sim::new(sim::Config {
        seed: input.replay,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let config = env::shards::Config {
        name: DIR.into(),
        core: None,
    };
    let handle = node
        .clone()
        .shards()
        .start(config, move |tasks| async move {
            let config = block::Config::new(1 << 21).expect("the budget fits");
            let pool =
                Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
            let shard = Shard { node, tasks, pool };
            let built = build(&shard, &input).await;
            let file = shard
                .node
                .files()
                .open(Path::new(RING), Mode::Write)
                .await
                .expect("the ring is there");
            let room = edit(&file, &shard.pool, &input.edits, &built.room).await;
            drop(file);
            check(&shard, &input, &built, room).await;
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
});
