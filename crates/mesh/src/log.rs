//! The `raft` state of one region on disk: the hard state and the log entries.
//!
//! The log is the files `log-0`, `log-1`, and so on in one directory. A file holds
//! records back to back, then bytes that are not a record. One record is one
//! [`Log::write`]: a check, the format version, the record's number, the length of
//! the body, and the body. Record numbers count up from 0 through all files. A
//! record that does not fit in the rest of a file starts the next file.
//!
//! A power cut can tear only the last record, because a write is durable before the
//! next one starts. So the first place with no good record is the end of the log,
//! unless a good record with a later number comes after it: where the bad record's
//! length says, or at the start of the next file. A bad record with a damaged length
//! in the last file is read as the end.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::{Block, Pool};
use env::files::{self, File, Files, Mode};
use raft::{Data, Entry, Hard, Position, Term, Voters};
use types::digest::Digest;
use types::node;

const VERSION: u16 = 1;
/// The bytes of a record before its body: check, version, number, and length.
const HEADER: usize = 26;
const CHECK: usize = 8;
/// The length of a file, unless its first record needs more.
const SEGMENT: u64 = 1 << 20;
/// The most bytes in one block of a read or a write.
const CHUNK: usize = 64 << 10;

const NO_HARD: u8 = 0;
const HARD: u8 = 1;
const HARD_WITH_VOTE: u8 = 2;
const EMPTY: u8 = 0;
const BYTES: u8 = 1;
const VOTERS: u8 = 2;

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
    // Where the next record goes in `file`.
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
    /// - [`Error::Pool`] when `pool` has no block of up to 64 KiB.
    /// - [`Error::Corrupt`] when a record is not valid and is not a torn end.
    /// - [`Error::Version`] when a record has another format version.
    pub(crate) async fn open(
        files: Files,
        dir: PathBuf,
        pool: Rc<Pool>,
    ) -> Result<(Self, Stored), Error> {
        let mut open = Vec::new();
        let mut segments = Vec::new();
        loop {
            let path = path(&dir, wide(open.len()));
            match files.open(&path, Mode::Write).await {
                Ok(file) => {
                    segments.push(read(&file, &pool).await?);
                    open.push(file);
                }
                Err(files::Error::NotFound { .. }) => break,
                Err(error) => return Err(error.into()),
            }
        }
        let scan = scan(&segments)
            .map_err(|(number, fault)| fault.at(path(&dir, wide(number))))?;
        let number = wide(scan.segment);
        if scan.spare {
            files.remove(&path(&dir, number.saturating_add(1))).await?;
            files.sync_dir(&dir).await?;
        }
        let file = if let Some(file) = open.into_iter().nth(scan.segment) {
            file
        } else {
            files.create_dir(&dir).await?;
            files
                .sync_dir(dir.parent().unwrap_or(Path::new("")))
                .await?;
            let mode = Mode::Create { len: SEGMENT };
            let file = files.open(&path(&dir, 0), mode).await?;
            files.sync_dir(&dir).await?;
            file
        };
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
    /// Do not drop the future before it ends. After an error or a drop, the log does
    /// not know where its end is: open it again.
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
            .chunks(CHUNK)
            .map(|chunk| {
                let mut block = self.pool.alloc(chunk.len())?;
                block.copy_from_slice(chunk);
                Ok(block.freeze())
            })
            .collect::<Result<Vec<Block>, Error>>()?;
        if self.offset.saturating_add(len) > self.file.len() {
            let number = self.number.saturating_add(1);
            let mode = Mode::Create {
                len: len.max(SEGMENT),
            };
            let file = self.files.open(&path(&self.dir, number), mode).await?;
            self.files.sync_dir(&self.dir).await?;
            (self.file, self.number, self.offset) = (file, number, 0);
        }
        self.file.write_at(self.offset, &parts).await?;
        self.file.sync().await?;
        self.offset = self.offset.saturating_add(len);
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

async fn read(file: &File, pool: &Pool) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while wide(bytes.len()) < file.len() {
        let offset = wide(bytes.len());
        let rest = file.len().saturating_sub(offset);
        let len = usize::try_from(rest).map_or(CHUNK, |rest| rest.min(CHUNK));
        let block = file.read_at(offset, pool.alloc(len)?).await?;
        bytes.extend_from_slice(&block);
    }
    Ok(bytes)
}

// Why `scan` refused a file.
#[derive(Debug, PartialEq, Eq)]
enum Fault {
    Corrupt { offset: usize },
    Version { found: u16 },
}

impl Fault {
    fn at(self, path: PathBuf) -> Error {
        match self {
            Self::Corrupt { offset } => Error::Corrupt {
                path,
                offset: wide(offset),
            },
            Self::Version { found } => Error::Version { path, found },
        }
    }
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

// Reads the records of the files in order. Gives the file of a fault with it.
fn scan(segments: &[Vec<u8>]) -> Result<Scan, (usize, Fault)> {
    let mut stored = Stored::default();
    let mut next = 0_u64;
    let mut segment = 0_usize;
    loop {
        let mut rest = segments.get(segment).map_or(&[][..], Vec::as_slice);
        let len = rest.len();
        let offset = |rest: &[u8]| len.saturating_sub(rest.len());
        while let Some(record) = record(rest, next) {
            let fault = match record.version {
                VERSION => Fault::Corrupt {
                    offset: offset(rest),
                },
                found => return Err((segment, Fault::Version { found })),
            };
            apply(&mut stored, record.body).ok_or((segment, fault))?;
            rest = record.after;
            next = next.saturating_add(1);
        }
        let offset = offset(rest);
        // A record with the number after `next` was written after record `next` was
        // durable, so record `next` is lost, not torn.
        let later = next.saturating_add(1);
        let follow = segments.get(segment.saturating_add(1));
        if claimed_body(rest).is_some_and(|(_, after)| good(after, later))
            || follow.is_some_and(|bytes| good(bytes, later))
        {
            return Err((segment, Fault::Corrupt { offset }));
        }
        match follow {
            Some(bytes) if good(bytes, next) => segment = segment.saturating_add(1),
            // A file is made only when the file before it has a durable record.
            Some(_) if segments.len() > segment.saturating_add(2) => {
                let fault = Fault::Corrupt { offset: 0 };
                return Err((segment.saturating_add(1), fault));
            }
            spare => {
                return Ok(Scan {
                    stored,
                    segment,
                    offset,
                    next,
                    spare: spare.is_some(),
                });
            }
        }
    }
}

fn good(bytes: &[u8], number: u64) -> bool {
    record(bytes, number).is_some()
}

// The check of a record: the first bytes of the digest of all that follows it.
fn check(bytes: &[u8]) -> [u8; CHECK] {
    let digest = Digest::of(bytes).0;
    *digest
        .first_chunk()
        .expect("invariant: a digest has 32 bytes")
}

// A record that passed its check.
struct Record<'a> {
    version: u16,
    body: &'a [u8],
    // The bytes after the record.
    after: &'a [u8],
}

// The record at the start of `bytes`, when it passes its check and has `number`.
fn record(bytes: &[u8], number: u64) -> Option<Record<'_>> {
    let (body, after) = claimed_body(bytes)?;
    let checked = bytes.get(CHECK..HEADER.saturating_add(body.len()))?;
    let mut header = checked;
    let version = u16::from_le_bytes(take(&mut header)?);
    let found = u64::from_le_bytes(take(&mut header)?);
    (bytes.starts_with(&check(checked)) && found == number).then_some(Record {
        version,
        body,
        after,
    })
}

// Splits `bytes` after the body that its header claims. It checks nothing.
fn claimed_body(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut rest = bytes.get(HEADER.saturating_sub(8)..)?;
    let len = usize::try_from(u64::from_le_bytes(take(&mut rest)?)).ok()?;
    rest.split_at_checked(len)
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
        body.extend(entry.at.term.0.to_le_bytes());
        body.extend(entry.at.index.to_le_bytes());
        match &entry.data {
            Data::Empty => body.push(EMPTY),
            Data::Bytes(bytes) => {
                body.push(BYTES);
                body.extend(wide(bytes.len()).to_le_bytes());
                body.extend(bytes);
            }
            Data::Voters(voters) => {
                body.push(VOTERS);
                for keys in [&voters.incoming, &voters.outgoing] {
                    body.extend(wide(keys.len()).to_le_bytes());
                    body.extend(
                        keys.iter().flat_map(|key| key.as_u128().to_le_bytes()),
                    );
                }
            }
        }
    }
    let mut checked = Vec::with_capacity(HEADER.saturating_add(body.len()));
    checked.extend(VERSION.to_le_bytes());
    checked.extend(number.to_le_bytes());
    checked.extend(wide(body.len()).to_le_bytes());
    checked.extend(body);
    let mut record = check(&checked).to_vec();
    record.extend(checked);
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
        let term = Term(u64::from_le_bytes(take(body)?));
        let index = u64::from_le_bytes(take(body)?);
        let data = match u8::from_le_bytes(take(body)?) {
            EMPTY => Data::Empty,
            BYTES => {
                let len = usize::try_from(u64::from_le_bytes(take(body)?)).ok()?;
                let (bytes, rest) = body.split_at_checked(len)?;
                *body = rest;
                Data::Bytes(bytes.to_vec())
            }
            VOTERS => Data::Voters(Voters {
                incoming: keys(body)?,
                outgoing: keys(body)?,
            }),
            _ => return None,
        };
        if std::mem::take(&mut first) {
            let keep = usize::try_from(index.checked_sub(1)?).ok()?;
            stored.entries.truncate(keep);
        }
        let at = Position { term, index };
        stored.entries.push(Entry { at, data });
    }
    Some(())
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = bytes.split_first_chunk()?;
    *bytes = rest;
    Some(*head)
}

fn key(bytes: &mut &[u8]) -> Option<node::Key> {
    take(bytes).map(|key| node::Key::from_u128(u128::from_le_bytes(key)))
}

fn keys(bytes: &mut &[u8]) -> Option<BTreeSet<node::Key>> {
    let count = u64::from_le_bytes(take(bytes)?);
    (0..count).map(|_| key(bytes)).collect()
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use env::files::Operation;
    use proptest::prelude::*;
    use sim::{Crash, Sim};
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
            at: Position { term, index },
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
                starts.push(log.offset);
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

    #[test]
    fn refuses_a_record_that_passes_its_check_with_a_body_it_cannot_read() {
        let (mut sim, node) = sim(0);
        on(&mut sim, &node, |node| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 4)]);
            // The kind of the entry.
            record[HEADER + 17] = 9;
            let check = check(&record[CHECK..]);
            record[..CHECK].copy_from_slice(&check);
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
            let check = check(&record[CHECK..]);
            record[..CHECK].copy_from_slice(&check);
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
        let error = on(&mut sim, &node, |node| async move {
            drop(open(&node).await.unwrap());
            let config = block::Config { budget: 4096 };
            let memory = block::Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            let largest = pool.largest();
            let error = Log::open(node.files(), DIR.into(), pool).await.unwrap_err();
            (error, largest)
        });
        let expected = block::Error::TooLarge {
            requested: CHUNK,
            largest: error.1,
        };
        assert_eq!(error.0, Error::Pool(expected));
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
            let record = record(&bytes, number).unwrap();
            prop_assert_eq!((record.version, record.after), (VERSION, &[][..]));
            let mut stored = Stored::default();
            prop_assert_eq!(apply(&mut stored, record.body), Some(()));
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
            prop_assert!(record(&bytes, 3).is_none());
        }

        #[test]
        fn any_bytes_scan_without_a_panic(
            files in prop::collection::vec(
                prop::collection::vec(any::<u8>(), 0..128),
                0..4,
            ),
        ) {
            drop(scan(&files));
        }
    }
}
