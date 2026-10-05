//! The `raft` state of one region on disk: the hard state and the log entries.
//!
//! The log is the files `log-0`, `log-1`, and so on in one directory. A file holds
//! records back to back, then zeros. One record is one [`Log::write`]: a header, then
//! the body. The header holds its own check, the format version, the record's number,
//! the length of the body, and the check of the body. Record numbers count up from 0
//! through all files. A record that does not fit in the rest of a file starts the
//! next file.
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

use crate::entry::{self, key, take};

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
}

impl Log {
    /// Opens the log in `dir`, and makes `dir` and an empty log when it has none. The
    /// parent of `dir` must be there. Returns the log and what it holds. A torn record
    /// at the end, which a crash leaves, is dropped.
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
                files
                    .sync_dir(dir.parent().unwrap_or(Path::new("")))
                    .await?;
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
            files.sync_dir(&dir).await?;
        }
        let file = if let Some(file) = open.into_iter().nth(scan.segment) {
            file
        } else {
            let mode = Mode::Create { len: SEGMENT };
            let file = files.open(&path(&dir, 0), mode).await?;
            files.sync_dir(&dir).await?;
            file
        };
        let tail = segments
            .get(scan.segment)
            .and_then(|bytes| bytes.get(scan.offset..));
        if tail.is_some_and(|tail| tail.iter().any(|&byte| byte != 0)) {
            zero(&file, wide(scan.offset), &pool).await?;
        }
        let log = Self {
            files,
            dir,
            pool,
            file,
            number,
            offset: wide(scan.offset),
            next: scan.next,
        };
        Ok((log, scan.stored))
    }

    /// Writes `hard`, when it is given, and `entries` as one record. The entries
    /// replace each entry at or after the index of the first one. Both are durable
    /// when the call returns: a crash before then keeps both or neither. A call with
    /// nothing to write does nothing.
    ///
    /// After [`Error::Files`], open the log again: a failed sync poisons the file.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a file call fails, and [`Error::Pool`] when the pool has
    /// no blocks for the record.
    pub(crate) async fn write(
        &mut self,
        hard: Option<Hard>,
        entries: &[Entry],
    ) -> Result<(), Error> {
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
        let mut start = wide(start(narrow(self.offset)));
        if start.saturating_add(len) > self.file.len() {
            let number = self.number.saturating_add(1);
            let mode = Mode::Create {
                len: len.max(SEGMENT),
            };
            let file = self.files.open(&path(&self.dir, number), mode).await?;
            self.files.sync_dir(&self.dir).await?;
            (self.file, self.number, start) = (file, number, 0);
        }
        self.file.write_at(start, &parts).await?;
        self.file.sync().await?;
        self.offset = start.saturating_add(len);
        self.next = self.next.saturating_add(1);
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

// Writes zeros from `from` to the end of `file`, durably.
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
    file.sync().await?;
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
        let after = path(dir, wide(segment.saturating_add(1)));
        match first {
            Some(At::Header(head)) if head.number == next => {
                segment = segment.saturating_add(1);
            }
            // A file that starts with a stale record or with garbage, and a file with
            // no record that is not the last one.
            Some(At::Header(_) | At::Garbage) => {
                return Err(Error::Corrupt {
                    path: after,
                    offset: 0,
                });
            }
            Some(At::End) if segments.len() > segment.saturating_add(2) => {
                return Err(Error::Corrupt {
                    path: after,
                    offset: 0,
                });
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
            body.extend(vote.iter().flat_map(|key| key.as_u128().to_le_bytes()));
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
                HARD_WITH_VOTE => Some(key(body)?),
                _ => None,
            };
            stored.hard = Hard { term, vote };
        }
        _ => return None,
    }
    let mut first = true;
    while !body.is_empty() {
        let entry = entry::decode(body)?;
        if std::mem::take(&mut first) {
            let keep = usize::try_from(entry.at.index.checked_sub(1)?).ok()?;
            stored.entries.truncate(keep);
        }
        stored.entries.push(entry);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

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

    /// Runs `body` on a new shard of `node` until it ends, and returns what it gives.
    fn on<T, F>(
        sim: &mut Sim,
        node: &sim::node::Node,
        body: impl FnOnce(sim::node::Node) -> F + Send + 'static,
    ) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let own = node.clone();
        let handle = node.shards().start(shard("log"), move |_| async move {
            *slot.lock().unwrap() = Some(body(own).await);
        });
        sim.run().unwrap();
        handle.unwrap().join().unwrap();
        out.lock().unwrap().take().expect("the shard gave a value")
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
    fn stored(sim: &mut Sim, node: &sim::node::Node) -> Result<Stored, Error> {
        on(sim, node, |node| async move {
            open(&node).await.map(|(_, stored)| stored)
        })
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
        on(&mut sim, &node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(Some(hard(1, Some(2))), &written[..2])
                .await
                .unwrap();
            log.write(None, &[]).await.unwrap();
            log.write(None, &written[2..]).await.unwrap();
            log.write(Some(hard(3, None)), &[]).await.unwrap();
        });
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
        on(&mut sim, &node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let first = [bytes(1, 1), bytes(2, 1), bytes(3, 1)];
            log.write(Some(hard(1, None)), &first).await.unwrap();
            let second = [entry(2, 2, Data::Empty)];
            log.write(None, &second).await.unwrap();
        });
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
            on(&mut sim, &node, move |node| async move {
                let (mut log, _) = open(&node).await.unwrap();
                let index = count + 1;
                let hard = hard(index, Some(1));
                log.write(Some(hard), &[bytes(index, 700)]).await.unwrap();
            });
            assert_eq!(stored(&mut sim, &node), Ok(after(count + 1)), "seed {seed}");
        }
        assert!(torn > 16, "only {torn} cuts were between two writes");
    }

    /// The true time between the cuts of two seeds in a row.
    const CUT_STEP: i64 = 12_500;

    fn file(name: &str) -> PathBuf {
        Path::new(DIR).join(name)
    }

    /// Writes three records of one entry each. Returns where each one starts.
    fn three(sim: &mut Sim, node: &sim::node::Node) -> Vec<u64> {
        on(sim, node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let mut starts = Vec::new();
            for index in 1..=3 {
                starts.push(wide(start(narrow(log.offset))));
                log.write(None, &[bytes(index, 100)]).await.unwrap();
            }
            starts
        })
    }

    #[test]
    fn drops_a_bad_record_at_the_end() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        let at = starts[2] + wide(HEADER) + 50;
        on(&mut sim, &node, move |node| async move {
            put(&node, "log-0", at, &[0xFF]).await;
        });
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
        on(&mut sim, &node, move |node| async move {
            put(&node, "log-0", at, &[0xFF]).await;
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(3, 10)]).await.unwrap();
        });
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 100), bytes(2, 100), bytes(3, 10)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    // A power cut keeps a header whole or not at all, so a damaged one is never
    // the torn end.
    #[test]
    fn refuses_a_bad_header_at_the_end() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        // The first byte of the body length of the last record.
        let at = starts[2] + wide(CHECK) + 10;
        on(&mut sim, &node, move |node| async move {
            put(&node, "log-0", at, &[127]).await;
        });
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
        let ends = on(&mut sim, &node, move |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let mut ends = Vec::new();
            for entry in &written {
                log.write(None, std::slice::from_ref(entry)).await.unwrap();
                ends.push(log.offset);
            }
            ends
        });
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
            on(&mut sim, &node, move |node| async move {
                drop(open(&node).await.unwrap());
                let mode = Mode::Create { len: 0 };
                drop(node.files().open(&file(name), mode).await.unwrap());
                node.files().sync_dir(Path::new(DIR)).await.unwrap();
            });
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
        on(&mut sim, &node, move |node| async move {
            put(&node, "log-0", at, &[0xFF]).await;
        });
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
        on(&mut sim, &node, |node| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 4)]);
            // The kind of the entry.
            record[HEADER + 17] = 9;
            sign(&mut record);
            put(&node, "log-0", 0, &record).await;
        });
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_record_of_another_format_version() {
        let (mut sim, node) = sim(0);
        on(&mut sim, &node, |node| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, Some(hard(1, None)), &[]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            sign(&mut record);
            put(&node, "log-0", 0, &record).await;
        });
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

    /// Writes a small record, one larger than a file, and a small one: three files.
    fn three_files(sim: &mut Sim, node: &sim::node::Node) -> Stored {
        on(sim, node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let entries = vec![bytes(1, 10), bytes(2, LARGE), bytes(3, 10)];
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
        let empty = encode(0, None, &[bytes(1, 0)]).len();
        let len = usize::try_from(SEGMENT).unwrap() - empty;
        let names = on(&mut sim, &node, move |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, len)]).await.unwrap();
            node.files().list(Path::new(DIR)).await.unwrap()
        });
        assert_eq!(names, [PathBuf::from("log-0")]);
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, len)],
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
            on(&mut sim, &node, move |node| async move {
                put(&node, name, wide(HEADER) + 5, &[0xFF]).await;
            });
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
        on(&mut sim, &node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, 10)]).await.unwrap();
            let mode = Mode::Create { len: 512 };
            drop(node.files().open(&file("log-1"), mode).await.unwrap());
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
        });
        let expected = on(&mut sim, &node, |node| async move {
            let (mut log, stored) = open(&node).await.unwrap();
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            assert_eq!(names, [PathBuf::from("log-0")]);
            let large = bytes(2, LARGE);
            log.write(None, std::slice::from_ref(&large)).await.unwrap();
            Stored {
                hard: Hard::default(),
                entries: stored.entries.into_iter().chain([large]).collect(),
            }
        });
        assert_eq!(expected.entries.len(), 2);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn gives_the_error_of_a_file_call_that_fails() {
        let (mut sim, node) = sim(0);
        let error = on(&mut sim, &node, |node| async move {
            let (mut log, _) = open(&node).await.unwrap();
            node.fail_file(&file("log-0"), Operation::Sync);
            log.write(None, &[bytes(1, 10)]).await.unwrap_err()
        });
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
        let (error, expected) = on(&mut sim, &node, |node| async move {
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
        });
        assert!(matches!(expected, block::Error::Exhausted { .. }));
        assert_eq!(error, Error::Pool(expected));
    }

    #[test]
    fn refuses_a_bad_header_in_a_file_before_a_file_with_records() {
        let (mut sim, node) = sim(0);
        let starts = three(&mut sim, &node);
        on(&mut sim, &node, |node| async move {
            let (mut log, stored) = open(&node).await.unwrap();
            assert_eq!(stored.entries.len(), 3);
            log.write(None, &[bytes(4, LARGE)]).await.unwrap();
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            assert_eq!(names, ["log-0", "log-1"].map(PathBuf::from));
        });
        sim.crash(&node, Crash::Power);
        // One bit of the body length of record 0, which is 126. Records 1 and 2
        // follow it in `log-0`, and record 3 is durable in `log-1`.
        let at = starts[0] + wide(CHECK) + 10;
        on(&mut sim, &node, move |node| async move {
            put(&node, "log-0", at, &[127]).await;
        });
        let result = stored(&mut sim, &node);
        let names = on(&mut sim, &node, |node| async move {
            node.files().list(Path::new(DIR)).await.unwrap()
        });
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: starts[0],
        };
        let kept = ["log-0", "log-1"].map(PathBuf::from).to_vec();
        assert_eq!((result, names), (Err(expected), kept));
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
