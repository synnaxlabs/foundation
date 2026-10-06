//! The `raft` state of one region on disk: the hard state and the log entries.
//!
//! The log is the files `log-0`, `log-1`, and so on in one directory. A file holds
//! records back to back, then zeros. One record is one [`Log::write`]: a header, then
//! the body. The header holds its own check, the format version, the record's number,
//! the length of the body, and the check of the body. Record numbers count up from 0
//! through all files. A record that does not fit in the rest of a file starts the
//! next file, or makes the file again, larger, when it holds no record.
//!
//! A header never crosses a 512-byte sector: a record whose header would cross one
//! starts at the next sector. A power cut keeps all or none of a sector, so a header
//! is whole or absent, and only the body of the last record can be torn. Where a
//! record should start, zeros are the end of the log, a header with a torn body is the
//! end too, and anything else is [`Error::Corrupt`]. Open zeroes the bytes after the
//! end, so a torn record leaves nothing that a later open reads as a header.

use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Pool};
use env::files::{self, File, Files, Mode};
use raft::{Entry, Hard, Term};
use types::digest::Digest;

use crate::bytes::{put_key, take, take_key};
use crate::entry;

const VERSION: u16 = 1;
const CHECK: usize = 8;
/// The bytes of a record before its body: the header check, the version, the number,
/// the body length, and the body check.
const HEADER: usize = 34;
/// The sector a header stays inside.
const SECTOR: usize = files::SECTOR;
/// The length of a file, unless its first record needs more.
const SEGMENT: u64 = 1 << 20;
/// The most bytes in one block of a read or a write.
const CHUNK: usize = 64 << 10;

const NO_HARD: u8 = 0;
const HARD: u8 = 1;
const HARD_WITH_VOTE: u8 = 2;

/// What a [`Log`] holds: the input of `raft::Raft::new` after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The last hard state written, or the default with none.
    pub(crate) hard: Hard,
    /// The entries from index 1.
    pub(crate) entries: Vec<Entry>,
}

/// Why a log call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// A file call failed.
    Files(files::Error),
    /// The pool has no block for a read or a write.
    Pool(block::Error),
    /// A record is not valid, and it is not the torn end that a crash leaves.
    Corrupt {
        /// The file.
        path: PathBuf,
        /// Where the record starts in the file.
        offset: u64,
    },
    /// A record has a format version that this build does not read.
    Version {
        /// The file.
        path: PathBuf,
        /// The version of the record.
        found: u16,
    },
    /// A file in the directory of the log that is not the next log file.
    Stray {
        /// The file.
        path: PathBuf,
    },
    /// A write failed or was dropped before it ended, so the end of the log is not
    /// known.
    Poisoned {
        /// The file that holds the end of the log.
        path: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Files(error) => error.fmt(f),
            Self::Pool(error) => error.fmt(f),
            Self::Corrupt { path, offset } => write!(
                f,
                "the record at byte {offset} of {} is not valid, and it is not a \
                 torn end of the log",
                path.display()
            ),
            Self::Version { path, found } => write!(
                f,
                "{} has a record of format version {found}, but this build reads \
                 version {VERSION}",
                path.display()
            ),
            Self::Stray { path } => write!(
                f,
                "{} is in the directory of the log, but it is not the next log file",
                path.display()
            ),
            Self::Poisoned { path } => write!(
                f,
                "a write of {} failed or was dropped; open the log again",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<files::Error> for Error {
    fn from(error: files::Error) -> Self {
        Self::Files(error)
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}

/// The hard state and the entries of one `raft` group, in the files of one directory.
#[derive(Debug)]
pub(crate) struct Log {
    files: Files,
    dir: PathBuf,
    pool: Rc<Pool>,
    // The file that holds the end of the log, and its number.
    file: File,
    number: u64,
    // Where the last record ends in `file`.
    offset: u64,
    // The number of the next record.
    next: u64,
    // The index of the last entry.
    last: u64,
    // A write failed or was dropped, so the end of the log is not known.
    poisoned: bool,
}

impl Log {
    /// Opens the log in `dir`, and makes `dir` and an empty log when it has none. The
    /// parent of `dir` must be there. Returns the log and what it holds, which is
    /// durable when the call returns. A torn record at the end, which a crash leaves,
    /// is dropped.
    ///
    /// # Errors
    ///
    /// - [`Error::Files`] when a file call fails.
    /// - [`Error::Pool`] when `pool` has no block for a read or a write.
    /// - [`Error::Corrupt`] when a record is not valid and is not a torn end.
    /// - [`Error::Version`] when a record has another format version.
    /// - [`Error::Stray`] when `dir` holds a file that is not the next log file.
    pub(crate) async fn open(
        files: Files,
        dir: PathBuf,
        pool: Rc<Pool>,
    ) -> Result<(Self, Stored), Error> {
        let names = match files.list(&dir).await {
            Ok(names) => names,
            Err(files::Error::NotFound { .. }) => {
                files.create_dir(&dir).await?;
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        };
        let mut open = Vec::new();
        let mut segments = Vec::new();
        for path in sequence(&dir, names)? {
            let file = files.open(&path, Mode::Write).await?;
            segments.push(read(&file, &pool).await?);
            open.push(file);
        }
        let scan = scan(&dir, &segments)?;
        let number = wide(scan.segment);
        if scan.spare {
            files.remove(&path(&dir, number.saturating_add(1))).await?;
        }
        let file = if let Some(file) = open.into_iter().nth(scan.segment) {
            file
        } else {
            let mode = Mode::Create { len: SEGMENT };
            files.open(&path(&dir, 0), mode).await?
        };
        let tail = segments
            .get(scan.segment)
            .and_then(|bytes| bytes.get(scan.offset..));
        if tail.is_some_and(|tail| tail.iter().any(|&byte| byte != 0)) {
            zero(&file, wide(scan.offset), &pool).await?;
        }
        // A crash before this open can leave the last record, a file, or `dir` with
        // no sync.
        file.sync().await?;
        files.sync_dir(&dir).await?;
        files
            .sync_dir(dir.parent().unwrap_or(Path::new("")))
            .await?;
        let log = Self {
            files,
            dir,
            pool,
            file,
            number,
            offset: wide(scan.offset),
            next: scan.next,
            last: wide(scan.stored.entries.len()),
            poisoned: false,
        };
        Ok((log, scan.stored))
    }

    /// Writes `hard`, when it is given, and `entries` as one record. The entries
    /// replace each entry at or after the index of the first one. Both are durable
    /// when the call returns: a crash before then keeps both or neither. A call with
    /// nothing to write does nothing.
    ///
    /// [`Error::Files`], or a drop of the future before it ends, poisons the log: open
    /// it again.
    ///
    /// # Errors
    ///
    /// - [`Error::Files`] when a file call fails.
    /// - [`Error::Pool`] when the pool has no blocks for the record.
    /// - [`Error::Poisoned`] after a failed or dropped write.
    ///
    /// # Panics
    ///
    /// When the first entry is not at an index from 1 to one past the last entry of
    /// the log, or the indexes of `entries` do not count up by one.
    pub(crate) async fn write(
        &mut self,
        hard: Option<Hard>,
        entries: &[Entry],
    ) -> Result<(), Error> {
        if self.poisoned {
            let path = path(&self.dir, self.number);
            return Err(Error::Poisoned { path });
        }
        assert!(
            follows(self.last, entries),
            "the entries of a write must follow the log"
        );
        if hard.is_none() && entries.is_empty() {
            return Ok(());
        }
        let record = encode(self.next, hard, entries);
        let len = wide(record.len());
        let parts = record
            .chunks(chunk(&self.pool))
            .map(|chunk| {
                let mut block = self.pool.alloc(chunk.len())?;
                block.copy_from_slice(chunk);
                Ok(block.freeze())
            })
            .collect::<Result<Vec<Block>, Error>>()?;
        self.poisoned = true;
        let mut start = wide(start(narrow(self.offset)));
        if start.saturating_add(len) > self.file.len() {
            // A file with no record is made again, larger: a scan refuses a file
            // with no record before another file.
            let empty = self.offset == 0;
            let number = self.number.saturating_add(u64::from(!empty));
            let path = path(&self.dir, number);
            if empty {
                self.files.remove(&path).await?;
            }
            let mode = Mode::Create {
                len: len.max(SEGMENT),
            };
            let file = self.files.open(&path, mode).await?;
            self.files.sync_dir(&self.dir).await?;
            (self.file, self.number, start) = (file, number, 0);
        }
        self.file.write_at(start, &parts).await?;
        self.file.sync().await?;
        self.offset = start.saturating_add(len);
        self.next = self.next.saturating_add(1);
        if let Some(entry) = entries.last() {
            self.last = entry.at.index;
        }
        self.poisoned = false;
        Ok(())
    }
}

fn path(dir: &Path, number: u64) -> PathBuf {
    dir.join(format!("log-{number}"))
}

fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}

// A file is read whole into memory, so each offset in it fits.
fn narrow(offset: u64) -> usize {
    usize::try_from(offset).expect("invariant: a file offset fits in memory")
}

// The most bytes in one block: `CHUNK`, or less when the pool has no such block.
fn chunk(pool: &Pool) -> usize {
    pool.largest().clamp(1, CHUNK)
}

// Where the record after one that ends at `end` starts: the next sector when the
// header would cross a sector boundary.
fn start(end: usize) -> usize {
    let room = SECTOR.saturating_sub(end.checked_rem(SECTOR).unwrap_or(0));
    if room < HEADER {
        end.saturating_add(room)
    } else {
        end
    }
}

// The log files among the `names` in `dir`, in number order. Each name must be
// `log-<n>` for an `n` below the count, once.
fn sequence(dir: &Path, names: Vec<PathBuf>) -> Result<Vec<PathBuf>, Error> {
    let mut paths = vec![None; names.len()];
    for name in names {
        let path = dir.join(&name);
        let number = name
            .to_str()
            .and_then(|name| name.strip_prefix("log-"))
            .and_then(|number| number.parse::<usize>().ok())
            .filter(|&number| self::path(dir, wide(number)) == path);
        match number.and_then(|number| paths.get_mut(number)) {
            Some(slot @ None) => *slot = Some(path),
            _ => return Err(Error::Stray { path }),
        }
    }
    Ok(paths.into_iter().flatten().collect())
}

async fn read(file: &File, pool: &Pool) -> Result<Vec<u8>, Error> {
    let chunk = chunk(pool);
    let mut bytes = Vec::new();
    while wide(bytes.len()) < file.len() {
        let offset = wide(bytes.len());
        let len = narrow(file.len().saturating_sub(offset)).min(chunk);
        let block = file.read_at(offset, pool.alloc(len)?).await?;
        bytes.extend_from_slice(&block);
    }
    Ok(bytes)
}

// Writes zeros from `from` to the end of `file`.
async fn zero(file: &File, from: u64, pool: &Pool) -> Result<(), Error> {
    let chunk = chunk(pool);
    let mut offset = from;
    while offset < file.len() {
        let len = narrow(file.len().saturating_sub(offset)).min(chunk);
        let mut block = pool.alloc(len)?;
        block.fill(0);
        file.write_at(offset, &[block.freeze()]).await?;
        offset = offset.saturating_add(wide(len));
    }
    Ok(())
}

// What the files hold, and where the log ends.
#[derive(Debug, PartialEq, Eq)]
struct Scan {
    stored: Stored,
    // The file that holds the end, and the offset of the end in it.
    segment: usize,
    offset: usize,
    // The number of the next record.
    next: u64,
    // A file after `segment` is there, with no record.
    spare: bool,
}

// Reads the records of the files in `dir`, which `segments` holds in order.
fn scan(dir: &Path, segments: &[Vec<u8>]) -> Result<Scan, Error> {
    let mut stored = Stored::default();
    let mut next = 0_u64;
    let mut segment = 0_usize;
    loop {
        let bytes = segments.get(segment).map_or(&[][..], Vec::as_slice);
        let file = path(dir, wide(segment));
        let (end, torn) = records(&mut stored, &file, bytes, &mut next)?;
        let first = segments
            .get(segment.saturating_add(1))
            .map(|bytes| header(bytes));
        // A record after a torn one, in its file or the next, was written after the
        // torn one was durable: the torn one is damaged.
        let follows = |claimed: usize| {
            bytes
                .get(start(claimed)..)
                .is_some_and(|rest| matches!(header(rest), At::Header(_)))
        };
        if torn.is_some_and(follows)
            || (torn.is_some() && matches!(first, Some(At::Header(_))))
            || matches!(first, Some(At::Header(ref head)) if head.number > next)
        {
            let offset = wide(start(end));
            return Err(Error::Corrupt { path: file, offset });
        }
        match first {
            // `records` refuses a next file that starts with a stale record or with
            // garbage.
            Some(At::Header(_) | At::Garbage) => segment = segment.saturating_add(1),
            // A next file with no record that is not the last file.
            Some(At::End) if segments.len() > segment.saturating_add(2) => {
                let path = path(dir, wide(segment.saturating_add(1)));
                return Err(Error::Corrupt { path, offset: 0 });
            }
            spare => {
                return Ok(Scan {
                    stored,
                    segment,
                    offset: end,
                    next,
                    spare: spare.is_some(),
                });
            }
        }
    }
}

// Applies the records of one file from record `next`. Returns where they end, and
// the claimed end of a record after them whose body is torn.
fn records(
    stored: &mut Stored,
    file: &Path,
    bytes: &[u8],
    next: &mut u64,
) -> Result<(usize, Option<usize>), Error> {
    let corrupt = |offset: usize| Error::Corrupt {
        path: file.to_path_buf(),
        offset: wide(offset),
    };
    let mut end = 0_usize;
    while let Some(rest) = bytes.get(start(end)..) {
        let at = start(end);
        let head = match header(rest) {
            At::End => break,
            At::Garbage => return Err(corrupt(at)),
            At::Header(head) => head,
        };
        if head.version != VERSION {
            let path = file.to_path_buf();
            return Err(Error::Version {
                path,
                found: head.version,
            });
        }
        let Some((body, after)) = head.body().filter(|_| head.number == *next) else {
            return Err(corrupt(at));
        };
        let claimed = bytes.len().saturating_sub(after.len());
        if check(body) != head.check {
            return Ok((end, Some(claimed)));
        }
        apply(stored, body).ok_or_else(|| corrupt(at))?;
        end = claimed;
        *next = next.saturating_add(1);
    }
    Ok((end, None))
}

// The check of some bytes: the first bytes of their digest.
fn check(bytes: &[u8]) -> [u8; CHECK] {
    let digest = Digest::of(bytes).0;
    *digest
        .first_chunk()
        .expect("invariant: a digest has 32 bytes")
}

// What is where a record should start.
enum At<'a> {
    // Zeros, or too few bytes for a header: the end of the records.
    End,
    // Bytes that are not zeros and not a header.
    Garbage,
    Header(Header<'a>),
}

// A header that passed its check.
struct Header<'a> {
    version: u16,
    number: u64,
    len: usize,
    // The check of the body.
    check: [u8; CHECK],
    // The bytes after the header.
    after: &'a [u8],
}

impl<'a> Header<'a> {
    // The body the header claims and the bytes after it, when the body fits.
    fn body(&self) -> Option<(&'a [u8], &'a [u8])> {
        self.after.split_at_checked(self.len)
    }
}

fn header(bytes: &[u8]) -> At<'_> {
    let Some((head, after)) = bytes.split_first_chunk::<HEADER>() else {
        return At::End;
    };
    if head.iter().all(|&byte| byte == 0) {
        return At::End;
    }
    match fields(head) {
        Some((version, number, len, check)) => At::Header(Header {
            version,
            number,
            len,
            check,
            after,
        }),
        None => At::Garbage,
    }
}

// The fields of a header that passes its check.
fn fields(head: &[u8]) -> Option<(u16, u64, usize, [u8; CHECK])> {
    let mut rest = head;
    let claimed: [u8; CHECK] = take(&mut rest)?;
    if claimed != check(rest) {
        return None;
    }
    let version = u16::from_le_bytes(take(&mut rest)?);
    let number = u64::from_le_bytes(take(&mut rest)?);
    let len = usize::try_from(u64::from_le_bytes(take(&mut rest)?)).ok()?;
    Some((version, number, len, take(&mut rest)?))
}

fn encode(number: u64, hard: Option<Hard>, entries: &[Entry]) -> Vec<u8> {
    let mut body = Vec::new();
    match hard {
        None => body.push(NO_HARD),
        Some(Hard { term, vote }) => {
            body.push(vote.map_or(HARD, |_| HARD_WITH_VOTE));
            body.extend(term.0.to_le_bytes());
            if let Some(vote) = vote {
                put_key(vote, &mut body);
            }
        }
    }
    for entry in entries {
        entry::encode(entry, &mut body);
    }
    let mut head = Vec::with_capacity(HEADER);
    head.extend(VERSION.to_le_bytes());
    head.extend(number.to_le_bytes());
    head.extend(wide(body.len()).to_le_bytes());
    head.extend(check(&body));
    let mut record = check(&head).to_vec();
    record.extend(head);
    record.extend(body);
    record
}

// Applies the body of one record. `None` when the body is not one that `encode`
// gives.
fn apply(stored: &mut Stored, mut body: &[u8]) -> Option<()> {
    let body = &mut body;
    match u8::from_le_bytes(take(body)?) {
        NO_HARD => {}
        kind @ (HARD | HARD_WITH_VOTE) => {
            let term = Term(u64::from_le_bytes(take(body)?));
            let vote = match kind {
                HARD_WITH_VOTE => Some(take_key(body)?),
                _ => None,
            };
            stored.hard = Hard { term, vote };
        }
        _ => return None,
    }
    let mut entries = Vec::new();
    while !body.is_empty() {
        entries.push(entry::decode(body)?);
    }
    if !follows(wide(stored.entries.len()), &entries) {
        return None;
    }
    if let Some(first) = entries.first() {
        let keep = usize::try_from(first.at.index.checked_sub(1)?).ok()?;
        stored.entries.truncate(keep);
    }
    stored.entries.extend(entries);
    Some(())
}

// Whether `entries` can follow a log whose last entry has index `last`: the first at
// an index from 1 to `last + 1`, and the others after it in order.
fn follows(last: u64, entries: &[Entry]) -> bool {
    entries.first().is_none_or(|first| {
        let from = first.at.index;
        (1..=last.saturating_add(1)).contains(&from)
            && (entries.iter().zip(from..))
                .all(|(entry, index)| entry.at.index == index)
    })
}

#[cfg(test)]
mod tests {
    use std::future::{pending, poll_fn};
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::Poll;

    use env::files::Operation;
    use proptest::prelude::*;
    use raft::{Data, Voters};
    use sim::{Crash, Sim};
    use types::node;
    use types::time::Span;

    use super::*;

    const DIR: &str = "mesh";

    fn shard(name: &str) -> env::shards::Config {
        env::shards::Config {
            name: name.into(),
            core: None,
        }
    }

    fn sim(seed: u64) -> (Sim, sim::node::Node) {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        (sim, node)
    }

    fn pool() -> Rc<Pool> {
        let config = block::Config { budget: 4 << 20 };
        let memory = block::Heap::new(config.reservation());
        Rc::new(Pool::new(config, memory))
    }

    async fn open(node: &sim::node::Node) -> Result<(Log, Stored), Error> {
        Log::open(node.files(), DIR.into(), pool()).await
    }

    /// What the log of `node` holds when it opens.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a sim error is a test failure, not a log error"
    )]
    fn stored(sim: &mut Sim, node: &sim::node::Node) -> Result<Stored, Error> {
        sim.run_on(node, |node, _| async move {
            open(&node).await.map(|(_, stored)| stored)
        })
        .unwrap()
    }

    /// Puts `bytes` at `offset` of a file of the log, durably, as a defect would.
    async fn put(node: &sim::node::Node, file: &str, offset: u64, bytes: &[u8]) {
        let path = Path::new(DIR).join(file);
        let file = node.files().open(&path, Mode::Write).await.unwrap();
        let mut block = pool().alloc(bytes.len()).unwrap();
        block.copy_from_slice(bytes);
        file.write_at(offset, &[block.freeze()]).await.unwrap();
        file.sync().await.unwrap();
    }

    fn key(id: u128) -> node::Key {
        node::Key::from_u128(id)
    }

    fn hard(term: u64, vote: Option<u128>) -> Hard {
        Hard {
            term: Term(term),
            vote: vote.map(key),
        }
    }

    fn entry(term: u64, index: u64, data: Data) -> Entry {
        let term = Term(term);
        Entry {
            at: raft::Position { term, index },
            data,
        }
    }

    /// An entry of term 1 with `len` bytes of `index`.
    fn bytes(index: u64, len: usize) -> Entry {
        let byte = u8::try_from(index % 251).unwrap();
        entry(1, index, Data::Bytes(vec![byte; len]))
    }

    fn voters(incoming: &[u128], outgoing: &[u128]) -> Data {
        let set = |ids: &[u128]| ids.iter().copied().map(key).collect();
        Data::Voters(Voters {
            incoming: set(incoming),
            outgoing: set(outgoing),
        })
    }

    #[test]
    fn a_new_log_is_empty() {
        let (mut sim, node) = sim(0);
        assert_eq!(stored(&mut sim, &node), Ok(Stored::default()));
    }

    #[test]
    fn gives_back_what_it_wrote_after_a_restart() {
        let (mut sim, node) = sim(0);
        let entries = vec![
            entry(1, 1, Data::Empty),
            entry(1, 2, voters(&[1, 2, 3], &[1, 2])),
            entry(2, 3, Data::Bytes(vec![7, 8, 9])),
            entry(2, 4, Data::Bytes(vec![])),
        ];
        let written = entries.clone();
        sim.run_on(&node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(Some(hard(1, Some(2))), &written[..2])
                .await
                .unwrap();
            log.write(None, &[]).await.unwrap();
            log.write(None, &written[2..]).await.unwrap();
            log.write(Some(hard(3, None)), &[]).await.unwrap();
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: hard(3, None),
            entries,
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn a_later_record_replaces_the_entries_from_its_first_index() {
        let (mut sim, node) = sim(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let first = [bytes(1, 1), bytes(2, 1), bytes(3, 1)];
            log.write(Some(hard(1, None)), &first).await.unwrap();
            let second = [entry(2, 2, Data::Empty)];
            log.write(None, &second).await.unwrap();
        })
        .unwrap();
        let expected = Stored {
            hard: hard(1, None),
            entries: vec![bytes(1, 1), entry(2, 2, Data::Empty)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    /// What the log holds after writes 1 to `count` of the power cut test.
    fn after(count: u64) -> Stored {
        Stored {
            hard: hard(count, (count > 0).then_some(1)),
            entries: (1..=count).map(|index| bytes(index, 700)).collect(),
        }
    }

    // Each write is a record of more than one sector, so a cut can tear it.
    #[test]
    fn a_power_cut_keeps_each_write_that_ended_and_all_or_none_of_the_next() {
        let mut torn = 0;
        for seed in 0..64 {
            let (mut sim, node) = sim(seed);
            let ended = Arc::new(AtomicU64::new(0));
            let count = Arc::clone(&ended);
            let own = node.clone();
            let handle = node.shards().start(shard("before"), move |_| async move {
                let (mut log, _) = open(&own).await.unwrap();
                for index in 1..=8 {
                    let hard = hard(index, Some(1));
                    log.write(Some(hard), &[bytes(index, 700)]).await.unwrap();
                    count.store(index, Ordering::Relaxed);
                }
                pending::<()>().await;
            });
            drop(handle.unwrap());
            sim.run_for(Span::from_nanos(CUT_STEP * i64::try_from(seed).unwrap()))
                .unwrap();
            let ended = ended.load(Ordering::Relaxed);
            sim.crash(&node, Crash::Power);

            let kept = stored(&mut sim, &node).unwrap();
            let count = wide(kept.entries.len());
            assert!(count == ended || count == ended + 1, "seed {seed}");
            assert_eq!(kept, after(count), "seed {seed}");
            torn += u64::from(ended > 0 && ended < 8);

            // The next write goes over what the cut left at the end.
            sim.run_on(&node, move |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                let index = count + 1;
                let hard = hard(index, Some(1));
                log.write(Some(hard), &[bytes(index, 700)]).await.unwrap();
            })
            .unwrap();
            assert_eq!(stored(&mut sim, &node), Ok(after(count + 1)), "seed {seed}");
        }
        assert!(torn > 16, "only {torn} cuts were between two writes");
    }

    /// The true time between the cuts of two seeds in a row.
    const CUT_STEP: i64 = 12_500;

    // `raft` acts on what open gives, so a power cut after open must keep it.
    #[test]
    fn a_power_cut_after_an_open_keeps_what_the_open_gave() {
        let mut lost = Vec::new();
        for seed in 0..64 {
            let (mut sim, node) = sim(seed);
            let own = node.clone();
            let handle = node.shards().start(shard("before"), move |_| async move {
                let (mut log, _) = open(&own).await.unwrap();
                for index in 1..=8 {
                    let hard = hard(index, Some(1));
                    log.write(Some(hard), &[bytes(index, 100)]).await.unwrap();
                }
                pending::<()>().await;
            });
            drop(handle.unwrap());
            sim.run_for(Span::from_nanos(CUT_STEP * i64::try_from(seed).unwrap()))
                .unwrap();
            sim.crash(&node, Crash::Process);
            let opened = stored(&mut sim, &node).unwrap();
            sim.crash(&node, Crash::Power);
            let kept = stored(&mut sim, &node).unwrap();
            if kept != opened {
                lost.push((seed, opened.hard, kept.hard));
            }
        }
        assert_eq!(lost, [], "(seed, hard that open gave, hard after a cut)");
    }

    // A failed open leaves the directory or `log-0` with no durable entry.
    #[test]
    fn a_write_after_a_failed_open_survives_a_power_cut() {
        for dir in ["", DIR] {
            let (mut sim, node) = sim(0);
            let error = sim
                .run_on(&node, move |node, _| async move {
                    node.fail_file(Path::new(dir), Operation::SyncDir);
                    let error = open(&node).await.unwrap_err();
                    let (mut log, _) = open(&node).await.unwrap();
                    log.write(Some(hard(1, None)), &[bytes(1, 10)])
                        .await
                        .unwrap();
                    error
                })
                .unwrap();
            let expected = files::Error::Io {
                path: dir.into(),
                operation: Operation::SyncDir,
                code: 5,
            };
            assert_eq!(error, Error::Files(expected));
            sim.crash(&node, Crash::Power);
            let expected = Stored {
                hard: hard(1, None),
                entries: vec![bytes(1, 10)],
            };
            assert_eq!(stored(&mut sim, &node), Ok(expected), "{dir:?}");
        }
    }

    fn file(name: &str) -> PathBuf {
        Path::new(DIR).join(name)
    }

    /// Writes three records of one entry each. Returns where each one starts.
    fn three(sim: &mut Sim, node: &sim::node::Node) -> Vec<u64> {
        sim.run_on(node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let mut starts = Vec::new();
            for index in 1..=3 {
                starts.push(wide(start(narrow(log.offset))));
                log.write(None, &[bytes(index, 100)]).await.unwrap();
            }
            starts
        })
        .unwrap()
    }

    #[test]
    fn drops_a_bad_record_at_the_end() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        let at = starts[2] + wide(HEADER) + 50;
        sim.run_on(&node, move |node, _| async move {
            put(&node, "log-0", at, &[0xFF]).await;
        })
        .unwrap();
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 100), bytes(2, 100)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn writes_over_a_dropped_record_with_no_stale_bytes_after_it() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        let at = starts[2] + wide(HEADER) + 50;
        sim.run_on(&node, move |node, _| async move {
            put(&node, "log-0", at, &[0xFF]).await;
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(3, 10)]).await.unwrap();
        })
        .unwrap();
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 100), bytes(2, 100), bytes(3, 10)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn an_open_with_no_torn_end_writes_nothing() {
        let (mut sim, node) = sim(0);
        three(&mut sim, &node);
        let opened = sim
            .run_on(&node, |node, _| async move {
                node.fail_file(&file("log-0"), Operation::WriteAt);
                open(&node).await.map(|(_, stored)| stored)
            })
            .unwrap();
        let expected = Stored {
            hard: Hard::default(),
            entries: (1..=3).map(|index| bytes(index, 100)).collect(),
        };
        assert_eq!(opened, Ok(expected));
    }

    // A power cut keeps a header whole or not at all, so a damaged one is never
    // the torn end.
    #[test]
    fn refuses_a_bad_header_at_the_end() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        // The first byte of the body length of the last record.
        let at = starts[2] + wide(CHECK) + 10;
        sim.run_on(&node, move |node, _| async move {
            put(&node, "log-0", at, &[127]).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: starts[2],
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn starts_a_record_at_the_next_sector_when_its_header_would_cross_one() {
        assert_eq!(
            [0, 478, 479, 512, 1000].map(start),
            [0, 478, 512, 512, 1024]
        );
        let (mut sim, node) = sim(0);
        // Each record is 61 bytes, so the ninth would start 24 bytes before 512.
        let entries: Vec<Entry> = (1..=9).map(|index| bytes(index, 1)).collect();
        let written = entries.clone();
        let ends = sim
            .run_on(&node, move |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                let mut ends = Vec::new();
                for entry in &written {
                    log.write(None, std::slice::from_ref(entry)).await.unwrap();
                    ends.push(log.offset);
                }
                ends
            })
            .unwrap();
        assert_eq!(ends[7..], [488, 573]);
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: Hard::default(),
            entries,
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn refuses_a_file_that_is_not_the_next_log_file() {
        for name in ["notes", "log-2", "log-01"] {
            let (mut sim, node) = sim(0);
            sim.run_on(&node, move |node, _| async move {
                drop(open(&node).await.unwrap());
                let mode = Mode::Create { len: 0 };
                drop(node.files().open(&file(name), mode).await.unwrap());
                node.files().sync_dir(Path::new(DIR)).await.unwrap();
            })
            .unwrap();
            let error = stored(&mut sim, &node).unwrap_err();
            assert_eq!(error, Error::Stray { path: file(name) }, "{name}");
            assert_eq!(
                error.to_string(),
                format!(
                    "mesh/{name} is in the directory of the log, but it is not the \
                     next log file"
                )
            );
        }
    }

    #[test]
    fn refuses_a_bad_record_with_a_good_one_after_it() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        let at = starts[1] + wide(HEADER) + 50;
        sim.run_on(&node, move |node, _| async move {
            put(&node, "log-0", at, &[0xFF]).await;
        })
        .unwrap();
        let error = stored(&mut sim, &node).unwrap_err();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: starts[1],
        };
        assert_eq!(error, expected);
        assert_eq!(
            error.to_string(),
            format!(
                "the record at byte {} of mesh/log-0 is not valid, and it is not a \
                 torn end of the log",
                starts[1]
            )
        );
    }

    /// Makes both checks of `record` match its bytes again.
    fn sign(record: &mut [u8]) {
        let body = check(&record[HEADER..]);
        record[HEADER - CHECK..HEADER].copy_from_slice(&body);
        let head = check(&record[CHECK..HEADER]);
        record[..CHECK].copy_from_slice(&head);
    }

    #[test]
    fn refuses_a_record_that_passes_its_check_with_a_body_it_cannot_read() {
        let (mut sim, node) = sim(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 4)]);
            // The kind of the entry.
            record[HEADER + 17] = 9;
            sign(&mut record);
            put(&node, "log-0", 0, &record).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_record_of_another_format_version() {
        let (mut sim, node) = sim(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, Some(hard(1, None)), &[]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            sign(&mut record);
            put(&node, "log-0", 0, &record).await;
        })
        .unwrap();
        let error = stored(&mut sim, &node).unwrap_err();
        let expected = Error::Version {
            path: file("log-0"),
            found: 2,
        };
        assert_eq!(error, expected);
        assert_eq!(
            error.to_string(),
            "mesh/log-0 has a record of format version 2, but this build reads \
             version 1"
        );
    }

    const LARGE: usize = 3 << 19;

    /// Writes a small record, one larger than a file, and two small ones: three files.
    fn three_files(sim: &mut Sim, node: &sim::node::Node) -> Stored {
        sim.run_on(node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let entries =
                vec![bytes(1, 10), bytes(2, LARGE), bytes(3, 10), bytes(4, 10)];
            for entry in &entries {
                log.write(None, std::slice::from_ref(entry)).await.unwrap();
            }
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            let expected = ["log-0", "log-1", "log-2"].map(PathBuf::from);
            assert_eq!(names, expected);
            Stored {
                hard: Hard::default(),
                entries,
            }
        })
        .unwrap()
    }

    #[test]
    fn a_record_that_does_not_fit_starts_a_file_that_holds_it() {
        let (mut sim, node) = sim(0);
        let expected = three_files(&mut sim, &node);
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn a_record_that_ends_at_the_end_of_a_file_stays_in_it() {
        let (mut sim, node) = sim(0);
        let empty = encode(1, None, &[bytes(2, 0)]).len();
        let (len, names) = sim
            .run_on(&node, move |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, 10)]).await.unwrap();
                let len = usize::try_from(SEGMENT).unwrap()
                    - start(narrow(log.offset))
                    - empty;
                log.write(None, &[bytes(2, len)]).await.unwrap();
                (len, node.files().list(Path::new(DIR)).await.unwrap())
            })
            .unwrap();
        assert_eq!(names, [PathBuf::from("log-0")]);
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 10), bytes(2, len)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn refuses_a_file_that_starts_with_a_record_before_the_next_one() {
        let records = [0, 1, 0].map(|number| encode(number, None, &[bytes(1, 10)]));
        let [first, second, stale] = records;
        let segments = [[first, second].concat(), stale];
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(scan(Path::new(DIR), &segments), Err(expected));
    }

    #[test]
    fn refuses_a_bad_last_record_of_a_file_before_a_file_with_records() {
        for (name, number) in [("log-0", 0), ("log-1", 1)] {
            let (mut sim, node) = sim(0);
            three_files(&mut sim, &node);
            sim.run_on(&node, move |node, _| async move {
                put(&node, name, wide(HEADER) + 5, &[0xFF]).await;
            })
            .unwrap();
            let expected = Error::Corrupt {
                path: file(name),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "file {number}");
        }
    }

    // A crash can leave the next file with no record. Its length can differ from
    // the one the next record needs.
    #[test]
    fn removes_a_file_with_no_record_after_the_end() {
        let (mut sim, node) = sim(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, 10)]).await.unwrap();
            let mode = Mode::Create { len: 512 };
            drop(node.files().open(&file("log-1"), mode).await.unwrap());
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
        })
        .unwrap();
        let expected = sim
            .run_on(&node, |node, _| async move {
                let (mut log, stored) = open(&node).await.unwrap();
                let names = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(names, [PathBuf::from("log-0")]);
                let large = bytes(2, LARGE);
                log.write(None, std::slice::from_ref(&large)).await.unwrap();
                Stored {
                    hard: Hard::default(),
                    entries: stored.entries.into_iter().chain([large]).collect(),
                }
            })
            .unwrap();
        assert_eq!(expected.entries.len(), 2);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn gives_the_error_of_a_file_call_that_fails() {
        let (mut sim, node) = sim(0);
        let error = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                node.fail_file(&file("log-0"), Operation::Sync);
                log.write(None, &[bytes(1, 10)]).await.unwrap_err()
            })
            .unwrap();
        let expected = files::Error::Io {
            path: file("log-0"),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(error, Error::Files(expected));
    }

    #[test]
    fn gives_the_error_of_a_pool_with_no_block_for_a_read() {
        let (mut sim, node) = sim(0);
        let (error, expected) = sim
            .run_on(&node, |node, _| async move {
                drop(open(&node).await.unwrap());
                let config = block::Config { budget: 4096 };
                let memory = block::Heap::new(config.reservation());
                let pool = Rc::new(Pool::new(config, memory));
                let held = pool.alloc(pool.largest()).unwrap();
                let expected = pool.alloc(chunk(&pool)).unwrap_err();
                let error = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                    .await
                    .unwrap_err();
                drop(held);
                (error, expected)
            })
            .unwrap();
        assert!(matches!(expected, block::Error::Exhausted { .. }));
        assert_eq!(error, Error::Pool(expected));
    }

    // Records 1 and 2 follow record 0 in `log-0`, and record 3 is durable in `log-1`.
    // The damage is one bit of the body length of record 0, which is 126, or zeros
    // over the header of record 2.
    #[test]
    fn refuses_a_bad_header_in_a_file_before_a_file_with_records() {
        let damages: [(usize, u64, &[u8]); 2] =
            [(0, wide(CHECK) + 10, &[127]), (2, 0, &[0; HEADER])];
        for (record, at, damage) in damages {
            let (mut sim, node) = sim(0);
            let starts = three(&mut sim, &node);
            sim.run_on(&node, |node, _| async move {
                let (mut log, stored) = open(&node).await.unwrap();
                assert_eq!(stored.entries.len(), 3);
                log.write(None, &[bytes(4, LARGE)]).await.unwrap();
                let names = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(names, ["log-0", "log-1"].map(PathBuf::from));
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            let at = starts[record] + at;
            sim.run_on(&node, move |node, _| async move {
                put(&node, "log-0", at, damage).await;
            })
            .unwrap();
            let result = stored(&mut sim, &node);
            let names = sim
                .run_on(&node, |node, _| async move {
                    node.files().list(Path::new(DIR)).await.unwrap()
                })
                .unwrap();
            let expected = Error::Corrupt {
                path: file("log-0"),
                offset: starts[record],
            };
            let kept = ["log-0", "log-1"].map(PathBuf::from).to_vec();
            assert_eq!((result, names), (Err(expected), kept), "record {record}");
        }
    }

    #[test]
    fn refuses_a_file_with_no_record_before_another_file() {
        let (mut sim, node) = sim(0);
        three(&mut sim, &node);
        sim.run_on(&node, |node, _| async move {
            for name in ["log-1", "log-2"] {
                let mode = Mode::Create { len: 512 };
                drop(node.files().open(&file(name), mode).await.unwrap());
            }
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    // A cut can keep the header of the first record of a file and tear its body.
    #[test]
    fn a_record_that_does_not_fit_replaces_a_file_with_no_record() {
        let (mut sim, node) = sim(0);
        let len = usize::try_from(SEGMENT).unwrap() - 1000;
        sim.run_on(&node, move |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, 2000)]).await.unwrap();
            log.write(None, &[bytes(2, len)]).await.unwrap();
            drop(log);
            put(&node, "log-1", 1024, &[0; 512]).await;
        })
        .unwrap();
        let expected = sim
            .run_on(&node, |node, _| async move {
                let (mut log, stored) = open(&node).await.unwrap();
                assert_eq!(stored.entries, [bytes(1, 2000)]);
                let large = bytes(2, LARGE);
                log.write(None, std::slice::from_ref(&large)).await.unwrap();
                let names = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(names, ["log-0", "log-1"].map(PathBuf::from));
                Stored {
                    hard: Hard::default(),
                    entries: vec![bytes(1, 2000), large],
                }
            })
            .unwrap();
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn a_first_record_larger_than_a_file_is_in_log_0() {
        let (mut sim, node) = sim(0);
        let files = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, LARGE)]).await.unwrap();
                drop(log);
                let mut files = Vec::new();
                for name in node.files().list(Path::new(DIR)).await.unwrap() {
                    let path = Path::new(DIR).join(&name);
                    let len = node.files().open(&path, Mode::Read).await.unwrap().len();
                    files.push((name, len));
                }
                files
            })
            .unwrap();
        let len = wide(encode(0, None, &[bytes(1, LARGE)]).len());
        assert_eq!(files, [(PathBuf::from("log-0"), len)]);
    }

    #[test]
    fn refuses_a_write_after_a_failed_or_dropped_one() {
        let (mut sim, node) = sim(0);
        let dropped = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                let entries = [bytes(1, 10)];
                {
                    let mut write = pin!(log.write(None, &entries));
                    let poll = poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx))).await;
                    assert!(poll.is_pending());
                }
                log.write(None, &entries).await.unwrap_err()
            })
            .unwrap();
        let failed = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                node.fail_file(&file("log-0"), Operation::WriteAt);
                let error = log.write(None, &[bytes(1, 10)]).await.unwrap_err();
                (error, log.write(None, &[bytes(1, 10)]).await.unwrap_err())
            })
            .unwrap();
        let poisoned = Error::Poisoned {
            path: file("log-0"),
        };
        let io = Error::Files(files::Error::Io {
            path: file("log-0"),
            operation: Operation::WriteAt,
            code: 5,
        });
        assert_eq!(
            (dropped, failed),
            (poisoned.clone(), (io, poisoned.clone()))
        );
        assert_eq!(
            poisoned.to_string(),
            "a write of mesh/log-0 failed or was dropped; open the log again"
        );
    }

    #[test]
    fn refuses_a_record_whose_entries_do_not_follow_the_log() {
        let cases = [
            vec![bytes(0, 1)],
            vec![bytes(2, 1)],
            vec![bytes(1, 1), bytes(3, 1)],
        ];
        for entries in cases {
            let segments = [encode(0, None, &entries)];
            let expected = Error::Corrupt {
                path: file("log-0"),
                offset: 0,
            };
            let indexes = entries.iter().map(|entry| entry.at.index);
            let indexes = indexes.collect::<Vec<_>>();
            assert_eq!(
                scan(Path::new(DIR), &segments),
                Err(expected),
                "{indexes:?}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "the entries of a write must follow the log")]
    fn panics_on_a_write_whose_entries_do_not_follow_the_log() {
        let (mut sim, node) = sim(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, 1)]).await.unwrap();
            log.write(None, &[bytes(3, 1)]).await.unwrap();
        })
        .unwrap();
    }

    fn data() -> impl Strategy<Value = Data> {
        let keys = || prop::collection::btree_set(any::<u128>().prop_map(key), 0..4);
        prop_oneof![
            Just(Data::Empty),
            prop::collection::vec(any::<u8>(), 0..64).prop_map(Data::Bytes),
            (keys(), keys()).prop_map(|(incoming, outgoing)| {
                Data::Voters(Voters { incoming, outgoing })
            }),
        ]
    }

    /// Entries from index 1, in terms that do not matter to the log.
    fn entries() -> impl Strategy<Value = Vec<Entry>> {
        prop::collection::vec((any::<u64>(), data()), 0..8).prop_map(|entries| {
            (1..)
                .zip(entries)
                .map(|(index, (term, data))| entry(term, index, data))
                .collect()
        })
    }

    fn hards() -> impl Strategy<Value = Option<Hard>> {
        prop::option::of((any::<u64>(), prop::option::of(any::<u128>())))
            .prop_map(|hard| hard.map(|(term, vote)| self::hard(term, vote)))
    }

    proptest! {
        #[test]
        fn a_record_gives_back_what_it_was_made_from(
            number: u64,
            hard in hards(),
            entries in entries(),
        ) {
            let bytes = encode(number, hard, &entries);
            let At::Header(head) = header(&bytes) else {
                panic!("a record starts with a header");
            };
            prop_assert_eq!((head.version, head.number), (VERSION, number));
            let (body, after) = head.body().unwrap();
            prop_assert_eq!((check(body), after), (head.check, &[][..]));
            let mut stored = Stored::default();
            prop_assert_eq!(apply(&mut stored, body), Some(()));
            let hard = hard.unwrap_or_default();
            prop_assert_eq!(stored, Stored { hard, entries });
        }

        #[test]
        fn a_record_with_one_changed_bit_is_not_a_record(
            hard in hards(),
            entries in entries(),
            at: prop::sample::Index,
            bit in 0..8_u8,
        ) {
            let mut bytes = encode(3, hard, &entries);
            let at = at.index(bytes.len());
            bytes[at] ^= 1 << bit;
            let valid = match header(&bytes) {
                At::Header(head) => head.body().is_some_and(|(body, _)| {
                    head.number == 3 && check(body) == head.check
                }),
                At::End | At::Garbage => false,
            };
            prop_assert!(!valid);
        }

        #[test]
        fn any_bytes_scan_without_a_panic(
            files in prop::collection::vec(
                prop::collection::vec(any::<u8>(), 0..128),
                0..4,
            ),
        ) {
            drop(scan(Path::new(DIR), &files));
        }
    }
}
