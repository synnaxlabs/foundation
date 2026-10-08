//! The `raft` state of one region on disk: the hard state and the log entries.
//!
//! The log is the files `log-0`, `log-1`, and so on in one directory. A file holds
//! records back to back, then zeros. One record is one write of the log: a header,
//! then the body. The header holds its own check, the format version, the record's
//! number, the length of the body, and the check of the body. Record numbers count up
//! from 0 through all files. A record that does not fit in the rest of a file starts
//! the next file, or makes the file again, larger, when it holds no record.
//!
//! The directory also holds the file `lock`, which has no bytes. A log holds it open
//! for writing while it lives, so a second open of the directory gives
//! [`files::Error::Busy`] on it.
//!
//! A header never crosses a 512-byte sector: a record whose header would cross one
//! starts at the next sector. A power cut keeps all or none of a sector, so a header
//! is whole or absent, and only the body of the last record can be torn. Where a
//! record should start, zeros are the end of the log, a header with a torn body is the
//! end too, and anything else is [`Error::Corrupt`]. Open writes again the end file
//! that it finds, the records as read and then zeros, in blocks of whole sectors. So a
//! torn record leaves nothing that a later open reads as a header, and a record whose
//! sync failed is durable.
//!
//! A write puts a record in blocks, one block of the pool at a time, from the end of
//! the record to its start. Each block but the one at the end of the record ends at a
//! multiple of the block size in the file, so no two blocks share a sector. The block
//! with the header is the last that it writes. So when the pool gives no block for a
//! part, the log has no header of the record, and the bytes of the parts after it stay
//! after the end of the log. The next write puts zeros over them, and syncs, before it
//! writes its record.

use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use block::Pool;
use env::files::{self, File, Files, Mode};
use raft::{Entry, Hard, Term};
use types::digest::Digest;

use crate::bytes::{
    block, put_optional_key, put_optional_proof, take, take_bool, take_key, take_proof,
};
use crate::entry;

const VERSION: u16 = 1;
const CHECK: usize = 8;
/// The bytes of a record before its body: the header check, the version, the number,
/// the body length, and the body check.
const HEADER: usize = 34;
/// The sector a header stays inside.
const SECTOR: usize = files::SECTOR;
/// The length of a file, unless the record that the log made it for needs more.
const SEGMENT: u64 = 1 << 20;
/// The most bytes in one block of a read or a write.
const CHUNK: usize = 64 << 10;
/// The name of the file that a log holds open while it lives.
const LOCK: &str = "lock";

const NO_HARD: u8 = 0;
const HARD: u8 = 1;

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
pub enum Error {
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
    /// A file in the directory of the log that is not the lock and not the next log
    /// file.
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
    // Where the bytes that a write put before the pool stopped it start and end in
    // `file`.
    stale: Option<(u64, u64)>,
    // The number of the next record.
    next: u64,
    // The index of the last entry.
    last: u64,
    // A write failed or was dropped, so the end of the log is not known.
    poisoned: bool,
    // Held so that a second open of `dir` fails. A write that starts a file lets go
    // of `file`, so `file` does not do that.
    _lock: File,
}

impl Log {
    /// Opens the log in `dir`, and makes `dir` and an empty log when it has none. The
    /// parent of `dir` must be there. Returns the log and what it holds, which is
    /// durable when the call returns. A torn record at the end, which a crash leaves,
    /// is dropped.
    ///
    /// One log at a time holds `dir`, from its open until it drops. A file call of a
    /// write that was dropped can end after that.
    ///
    /// # Errors
    ///
    /// - [`Error::Files`] when a file call fails. It holds [`files::Error::Busy`] on
    ///   the file `lock` of `dir` while another log holds `dir`.
    /// - [`Error::Pool`] when the largest block of `pool` is less than one sector of
    ///   512 bytes, or `pool` has no block for a read or a write.
    /// - [`Error::Corrupt`] when a record is not valid and is not a torn end.
    /// - [`Error::Version`] when a record has another format version. A record that
    ///   starts right after a torn one, or at the start of the next file, gives
    ///   [`Error::Corrupt`] at the torn one, whatever its version.
    /// - [`Error::Stray`] when `dir` holds a file that is not the lock and not the
    ///   next log file.
    pub(crate) async fn open(
        files: Files,
        dir: PathBuf,
        pool: Rc<Pool>,
    ) -> Result<(Self, Stored), Error> {
        let largest = pool.largest();
        if largest < SECTOR {
            let requested = SECTOR;
            return Err(Error::Pool(block::Error::TooLarge { requested, largest }));
        }
        files.create_dir(&dir).await?;
        let lock = files.open(&dir.join(LOCK), Mode::Create { len: 0 }).await?;
        let names = files.list(&dir).await?;
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
            // A sync that failed in this boot leaves its writes in the cache, where
            // the read saw them, and a later sync does not write them. So the open
            // writes again what it read, with zeros after the last record.
            let bytes = segments
                .get_mut(scan.segment)
                .expect("invariant: the open read each file that it holds");
            bytes
                .get_mut(scan.offset..)
                .expect("invariant: the log ends in its end file")
                .fill(0);
            write_in_blocks(&file, &pool, 0, &mut &bytes[..]).await?;
            file
        } else {
            let mode = Mode::Create { len: SEGMENT };
            files.open(&path(&dir, 0), mode).await?
        };
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
            stale: None,
            next: scan.next,
            last: wide(scan.stored.entries.len()),
            poisoned: false,
            _lock: lock,
        };
        Ok((log, scan.stored))
    }

    /// Writes `hard`, when it is given, and `entries` as one record. The entries
    /// replace each entry at or after the index of the first one. Both are durable
    /// when the call returns: a crash before then keeps both or neither. A call with
    /// nothing to write does nothing.
    ///
    /// The write holds one block of the pool at a time, of 64 KiB at most.
    /// [`Error::Files`], or a drop of the future before it ends, poisons the log: open
    /// it again. After [`Error::Pool`] the log holds what it held, and a later write
    /// may succeed.
    ///
    /// # Errors
    ///
    /// - [`Error::Files`] when a file call fails.
    /// - [`Error::Pool`] when the pool has no block for a part of the record.
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
        self.poisoned = true;
        let result = self.append(hard, entries).await;
        self.poisoned = !matches!(result, Ok(()) | Err(Error::Pool(_)));
        result
    }

    // Writes one record after the last one. A pool error leaves each record as it
    // was. It can leave the bytes that `stale` names, and the end of the log in the
    // file that it made for the record.
    async fn append(
        &mut self,
        hard: Option<Hard>,
        entries: &[Entry],
    ) -> Result<(), Error> {
        if let Some((at, end)) = self.stale {
            // A scan reads bytes that stay after a later record as its next header.
            // The zeros get their own sync: with one sync, a power cut can keep the
            // record and not the zeros.
            let zeros = vec![0; narrow(end.saturating_sub(at))];
            write_in_blocks(&self.file, &self.pool, at, &mut &zeros[..]).await?;
            self.file.sync().await?;
            self.stale = None;
        }
        let record = encode(self.next, hard, entries);
        let len = wide(record.len());
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
            (self.file, self.number, self.offset, start) = (file, number, 0, 0);
        }
        let end = start.saturating_add(len);
        let mut rest = &record[..];
        let written = write_in_blocks(&self.file, &self.pool, start, &mut rest).await;
        if written.is_err() && rest.len() < record.len() {
            self.stale = Some((start.saturating_add(wide(rest.len())), end));
        }
        written?;
        self.file.sync().await?;
        self.offset = end;
        self.next = self.next.saturating_add(1);
        if let Some(entry) = entries.last() {
            self.last = entry.at.index;
        }
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

// The most bytes in one block of a read or a write: `CHUNK`, or less when the pool has
// no such block. It is whole sectors, and an open refuses a pool that gives 0.
fn chunk(pool: &Pool) -> usize {
    let largest = pool.largest();
    largest.saturating_sub(largest % SECTOR).min(CHUNK)
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

// The log files among the `names` in `dir`, in number order. Each name but that of
// the lock must be `log-<n>` for an `n` below the count, once.
fn sequence(dir: &Path, mut names: Vec<PathBuf>) -> Result<Vec<PathBuf>, Error> {
    names.retain(|name| name != Path::new(LOCK));
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

// Writes `bytes` at `at` of `file`, one block at a time, from the end of `bytes` to
// its start, and takes each block that it wrote off the end of `bytes`. Each block but
// the one at the end ends at a multiple of the block size in the file, so no two
// blocks share a sector.
async fn write_in_blocks(
    file: &File,
    pool: &Pool,
    at: u64,
    bytes: &mut &[u8],
) -> Result<(), Error> {
    let chunk = chunk(pool);
    while !bytes.is_empty() {
        let end = narrow(at).saturating_add(bytes.len());
        let over = end
            .checked_rem(chunk)
            .expect("invariant: a block of the log is one sector or more");
        let len = if over == 0 { chunk } else { over }.min(bytes.len());
        let (rest, part) = bytes.split_at(bytes.len().saturating_sub(len));
        let block = block(pool, part)?;
        let offset = at.saturating_add(wide(rest.len()));
        file.write_at(offset, &[block]).await?;
        *bytes = rest;
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
        // The number of a header of another version has no meaning here. `records`
        // gives the error of that file.
        let known = match first {
            Some(At::Header(ref head)) if head.version == VERSION => Some(head.number),
            _ => None,
        };
        // A record right after a torn one, or at the start of the next file, was
        // written after the torn one was durable: the torn one is damaged. A write
        // fills each file before the next one, so a file with no record before a
        // file that is not empty is damaged, whatever the version of that file.
        let follows = |claimed: usize| {
            bytes
                .get(start(claimed)..)
                .is_some_and(|rest| matches!(header(rest), At::Header(_)))
        };
        if torn.is_some_and(follows)
            || (torn.is_some() && matches!(first, Some(At::Header(_))))
            || (end == 0
                && torn.is_none()
                && matches!(first, Some(At::Header(_) | At::Garbage)))
            || known.is_some_and(|number| number > next)
        {
            let offset = wide(start(end));
            return Err(Error::Corrupt { path: file, offset });
        }
        match first {
            // `records` refuses a next file that starts with a stale record or with
            // garbage.
            Some(At::Header(_) | At::Garbage) => segment = segment.saturating_add(1),
            // A next file with no record that is not the last file, or this file
            // when it has no record either. A torn record is the end of the log.
            Some(At::End) if segments.len() > segment.saturating_add(2) => {
                let held = end != 0 || torn.is_some();
                let empty = segment.saturating_add(usize::from(held));
                let path = path(dir, wide(empty));
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
        let end_of = |after: &[u8]| bytes.len().saturating_sub(after.len());
        let head = match header(rest) {
            At::End => break,
            At::Garbage => return Err(corrupt(at)),
            At::Header(head) => head,
        };
        let (body, after) = match head.read() {
            Record::Version(found) => {
                let path = file.to_path_buf();
                return Err(Error::Version { path, found });
            }
            Record::Torn { number, after } if number == *next => {
                return Ok((end, Some(end_of(after))));
            }
            Record::Whole {
                number,
                body,
                after,
            } if number == *next => (body, after),
            Record::Short | Record::Torn { .. } | Record::Whole { .. } => {
                return Err(corrupt(at));
            }
        };
        apply(stored, body).ok_or_else(|| corrupt(at))?;
        end = end_of(after);
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
    // Zeros, or no bytes: the end of the records.
    End,
    // Bytes that are not zeros and not a header, also too few for one.
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

// The record that a header starts.
enum Record<'a> {
    // A header of another format version.
    Version(u16),
    // A header that claims a body longer than the bytes after it.
    Short,
    // A body that fails its check, and the bytes after it.
    Torn {
        number: u64,
        after: &'a [u8],
    },
    // A body that passes its check, and the bytes after it.
    Whole {
        number: u64,
        body: &'a [u8],
        after: &'a [u8],
    },
}

impl<'a> Header<'a> {
    fn read(self) -> Record<'a> {
        if self.version != VERSION {
            return Record::Version(self.version);
        }
        let Some((body, after)) = self.after.split_at_checked(self.len) else {
            return Record::Short;
        };
        let number = self.number;
        if check(body) == self.check {
            Record::Whole {
                number,
                body,
                after,
            }
        } else {
            Record::Torn { number, after }
        }
    }
}

fn header(bytes: &[u8]) -> At<'_> {
    let zeros = |bytes: &[u8]| bytes.iter().all(|&byte| byte == 0);
    let Some((head, after)) = bytes.split_first_chunk::<HEADER>() else {
        return if zeros(bytes) { At::End } else { At::Garbage };
    };
    if zeros(head) {
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

pub(crate) fn encode(number: u64, hard: Option<Hard>, entries: &[Entry]) -> Vec<u8> {
    let mut body = Vec::new();
    match hard {
        None => body.push(NO_HARD),
        Some(hard) => {
            body.push(HARD);
            body.extend(hard.term.0.to_le_bytes());
            put_optional_key(hard.vote, &mut body);
            put_optional_key(hard.leader, &mut body);
            put_optional_proof(hard.proof.as_ref(), &mut body);
        }
    }
    entry::encode(entries, &mut body);
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
// gives, or its entries do not follow `stored`.
fn apply(stored: &mut Stored, bytes: &[u8]) -> Option<()> {
    let (hard, entries) = body(bytes)?;
    if !follows(wide(stored.entries.len()), &entries) {
        return None;
    }
    if let Some(hard) = hard {
        stored.hard = hard;
    }
    if let Some(first) = entries.first() {
        let keep = usize::try_from(first.at.index.checked_sub(1)?).ok()?;
        stored.entries.truncate(keep);
    }
    stored.entries.extend(entries);
    Some(())
}

// The hard state and the entries of a record body. `None` when the body is not one
// that `encode` gives.
fn body(mut bytes: &[u8]) -> Option<(Option<Hard>, Vec<Entry>)> {
    let body = &mut bytes;
    let hard = match u8::from_le_bytes(take(body)?) {
        NO_HARD => None,
        HARD => {
            let term = Term(u64::from_le_bytes(take(body)?));
            let vote = if take_bool(body)? {
                Some(take_key(body)?)
            } else {
                None
            };
            let leader = if take_bool(body)? {
                Some(take_key(body)?)
            } else {
                None
            };
            let proof = if take_bool(body)? {
                Some(take_proof(body)?)
            } else {
                None
            };
            Some(Hard {
                term,
                vote,
                leader,
                proof,
            })
        }
        _ => return None,
    };
    Some((hard, entry::decode(std::mem::take(body))?))
}

/// The number, hard state, and entries of `record`, the reverse of [`encode`], or
/// `None` when it is not one whole record of the format version this build writes.
/// The number of the record and whether its entries follow a log are not checked.
#[cfg(any(test, feature = "sim"))]
pub(crate) fn decode(record: &[u8]) -> Option<(u64, Option<Hard>, Vec<Entry>)> {
    let Some(Record::Whole {
        number,
        body: bytes,
        after: [],
    }) = self::record(record)
    else {
        return None;
    };
    let (hard, entries) = body(bytes)?;
    Some((number, hard, entries))
}

// The record at the start of `bytes`, or `None` when no header is there.
#[cfg(any(test, feature = "sim"))]
fn record(bytes: &[u8]) -> Option<Record<'_>> {
    match header(bytes) {
        At::Header(head) => Some(head.read()),
        At::End | At::Garbage => None,
    }
}

/// Makes the length and both checks of `record` match its bytes, with each byte after
/// the header in the body. Does nothing to fewer bytes than a header.
#[cfg(any(test, feature = "sim"))]
pub(crate) fn seal(record: &mut [u8]) {
    let Some((head, body)) = record.split_first_chunk_mut::<HEADER>() else {
        return;
    };
    let (fields, claimed) = head
        .split_last_chunk_mut::<CHECK>()
        .expect("invariant: a header ends with the body check");
    *claimed = check(body);
    let (_, len) = fields
        .split_last_chunk_mut::<8>()
        .expect("invariant: the length comes before the body check");
    *len = wide(body.len()).to_le_bytes();
    let (claimed, rest) = head
        .split_first_chunk_mut::<CHECK>()
        .expect("invariant: a header starts with its check");
    *claimed = check(rest);
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
    use raft::{Change, Data, Grant, Proof, Signature, Voters};
    use sim::{Crash, Sim};
    use types::node;
    use types::time::Span;

    use super::*;
    use crate::bytes::{PRESENT, VOTE};
    use crate::common::create_pool;

    const DIR: &str = "mesh";

    fn shard(name: &str) -> env::shards::Config {
        env::shards::Config {
            name: name.into(),
            core: None,
        }
    }

    fn create_node(seed: u64) -> (Sim, sim::node::Node) {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        (sim, node)
    }

    async fn open(node: &sim::node::Node) -> Result<(Log, Stored), Error> {
        Log::open(node.files(), DIR.into(), create_pool()).await
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
        let block = block(&create_pool(), bytes).unwrap();
        file.write_at(offset, &[block]).await.unwrap();
        file.sync().await.unwrap();
    }

    fn key(id: u128) -> node::Key {
        node::Key::from_u128(id)
    }

    fn hard(term: u64, vote: Option<u128>) -> Hard {
        Hard {
            term: Term(term),
            vote: vote.map(key),
            leader: None,
            proof: None,
        }
    }

    // Each voter signs with its key's low byte, 64 times.
    fn proof(grant: Grant, candidate: u128, voters: &[u128]) -> Proof {
        let signed = |&voter: &u128| {
            let signature = Signature([voter.to_le_bytes()[0]; 64]);
            (key(voter), Some(signature))
        };
        Proof {
            grant,
            candidate: key(candidate),
            voters: voters.iter().map(signed).collect(),
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

    // A change of node 1 with `signature`, signed as the log takes it.
    fn change(voters: Voters, signature: u8) -> Data {
        Data::Voters(Change {
            voters,
            votes: Proof {
                grant: Grant::Vote,
                candidate: key(1),
                voters: [(key(1), Some(Signature([1; 64])))].into(),
            },
            signature: Some(Signature([signature; 64])),
        })
    }

    fn voters(incoming: &[u128], outgoing: &[u128]) -> Data {
        let set = |ids: &[u128]| ids.iter().copied().map(key).collect();
        let voters = Voters {
            incoming: set(incoming),
            outgoing: set(outgoing),
        };
        change(voters, 2)
    }

    #[test]
    fn a_new_log_is_empty() {
        let (mut sim, node) = create_node(0);
        assert_eq!(stored(&mut sim, &node), Ok(Stored::default()));
    }

    #[test]
    fn gives_back_what_it_wrote_after_a_restart() {
        let (mut sim, node) = create_node(0);
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
        let (mut sim, node) = create_node(0);
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
            let (mut sim, node) = create_node(seed);
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
            let (mut sim, node) = create_node(seed);
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

    // A failed sync leaves its record in the cache, clean, and not on the disk.
    #[test]
    fn a_power_cut_keeps_what_an_open_after_a_failed_sync_gave() {
        let mut lost = Vec::new();
        let mut gave = 0_usize;
        for seed in 0..64 {
            let (mut sim, node) = create_node(seed);
            let expected = sim
                .run_on(&node, |node, _| async move {
                    let (mut log, _) = open(&node).await.unwrap();
                    log.write(Some(hard(1, Some(1))), &[bytes(1, 100)])
                        .await
                        .unwrap();
                    node.fail_file(&file("log-0"), Operation::Sync);
                    let error = log
                        .write(Some(hard(2, Some(1))), &[bytes(2, 100)])
                        .await
                        .unwrap_err();
                    assert_eq!(error, io("log-0", Operation::Sync));
                    drop(log);
                    let (mut log, mut opened) = open(&node).await.unwrap();
                    let next = bytes(wide(opened.entries.len()) + 1, 100);
                    opened.hard = hard(3, Some(1));
                    opened.entries.push(next.clone());
                    log.write(Some(opened.hard.clone()), &[next]).await.unwrap();
                    opened
                })
                .unwrap();
            // The entries are the first, the failed one or not, and the last.
            if expected.entries.len() == 3 {
                gave = gave.saturating_add(1);
            }
            sim.crash(&node, Crash::Power);
            let kept = stored(&mut sim, &node).unwrap();
            if kept != expected {
                lost.push((seed, expected.entries.len(), kept.entries.len()));
            }
        }
        assert_eq!(lost, [], "(seed, entries before the cut, entries after it)");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    // A failed open leaves the directory or `log-0` with no durable entry.
    #[test]
    fn a_write_after_a_failed_open_survives_a_power_cut() {
        for dir in ["", DIR] {
            let (mut sim, node) = create_node(0);
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
    fn create_three(sim: &mut Sim, node: &sim::node::Node) -> Vec<u64> {
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
        let (mut sim, node) = create_node(0);
        let starts = create_three(&mut sim, &node);
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
        let (mut sim, node) = create_node(0);
        let starts = create_three(&mut sim, &node);
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

    /// Damages the last record, which starts at `end`, fails the sync of one open,
    /// opens again, and cuts the power. Returns whether `log-0` then holds only
    /// zeros from `end`.
    fn zeros_after_a_failed_open(
        sim: &mut Sim,
        node: &sim::node::Node,
        end: u64,
    ) -> bool {
        let at = end.saturating_add(wide(HEADER)).saturating_add(50);
        sim.run_on(node, move |node, _| async move {
            put(&node, "log-0", at, &[0xFF]).await;
            node.fail_file(&file("log-0"), Operation::Sync);
            let error = open(&node).await.unwrap_err();
            assert_eq!(error, io("log-0", Operation::Sync));
            open(&node).await.unwrap();
        })
        .unwrap();
        sim.crash(node, Crash::Power);
        sim.run_on(node, move |node, _| async move {
            let log = file("log-0");
            let file = node.files().open(&log, Mode::Read).await.unwrap();
            let bytes = read(&file, &create_pool()).await.unwrap();
            bytes[narrow(end)..].iter().all(|&byte| byte == 0)
        })
        .unwrap()
    }

    const TORN: &str = "the runs with bytes after the end of the log";

    // The failed sync of an open leaves its zeros in the cache, clean, and the torn
    // record on the disk.
    #[test]
    fn a_power_cut_keeps_the_zeros_of_an_open_after_a_failed_open() {
        let mut torn = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            let starts = create_three(&mut sim, &node);
            if !zeros_after_a_failed_open(&mut sim, &node, starts[2]) {
                torn.push(run);
            }
        }
        assert_eq!(torn, [], "{TORN}");
    }

    // The torn record is the first, so each block that the open writes is zeros.
    #[test]
    fn a_power_cut_keeps_the_zeros_of_an_open_that_gave_no_record() {
        let mut torn = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, 700)]).await.unwrap();
            })
            .unwrap();
            if !zeros_after_a_failed_open(&mut sim, &node, 0) {
                torn.push(run);
            }
        }
        assert_eq!(torn, [], "{TORN}");
    }

    // The torn record starts in one block of the open's write and ends in the next.
    #[test]
    fn a_power_cut_keeps_the_zeros_of_an_open_across_two_blocks() {
        let mut torn = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            let end = sim
                .run_on(&node, |node, _| async move {
                    let (mut log, _) = open(&node).await.unwrap();
                    log.write(None, &[bytes(1, CHUNK - 700)]).await.unwrap();
                    let end = wide(start(narrow(log.offset)));
                    log.write(None, &[bytes(2, 700)]).await.unwrap();
                    end
                })
                .unwrap();
            if !zeros_after_a_failed_open(&mut sim, &node, end) {
                torn.push(run);
            }
        }
        assert_eq!(torn, [], "{TORN}");
    }

    // The torn record ends in the last block of the file.
    #[test]
    fn a_power_cut_keeps_the_zeros_of_an_open_to_the_last_block() {
        let mut torn = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, 15 * CHUNK + 1000)])
                    .await
                    .unwrap();
            })
            .unwrap();
            if !zeros_after_a_failed_open(&mut sim, &node, 0) {
                torn.push(run);
            }
        }
        assert_eq!(torn, [], "{TORN}");
    }

    #[test]
    fn an_open_gives_the_error_of_a_failed_write() {
        let (mut sim, node) = create_node(0);
        create_three(&mut sim, &node);
        let opened = sim
            .run_on(&node, |node, _| async move {
                node.fail_file(&file("log-0"), Operation::WriteAt);
                open(&node).await.map(|(_, stored)| stored)
            })
            .unwrap();
        let failed = files::Error::Io {
            path: file("log-0"),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(opened, Err(Error::Files(failed)));
    }

    fn io(name: &str, operation: Operation) -> Error {
        Error::Files(files::Error::Io {
            path: file(name),
            operation,
            code: 5,
        })
    }

    /// A pool whose largest block, 1,792 bytes, is 3.5 sectors.
    fn odd_pool() -> Rc<Pool> {
        let config = block::Config { budget: 2048 };
        let memory = block::Heap::new(config.reservation());
        let pool = Rc::new(Pool::new(config, memory));
        assert_eq!(pool.largest(), 1792);
        pool
    }

    async fn odd_open(node: &sim::node::Node) -> Result<(Log, Stored), Error> {
        Log::open(node.files(), DIR.into(), odd_pool()).await
    }

    #[test]
    fn a_block_is_the_most_whole_sectors_that_the_pool_gives() {
        assert_eq!(chunk(&odd_pool()), 3 * SECTOR);
        assert_eq!(chunk(&create_pool()), CHUNK);
    }

    // The header of the second record is at bytes 1,770 to 1,804, in one sector and
    // across byte 1,792. An open that writes zeros on it in two writes can leave
    // half of it after its failed sync and a power cut.
    #[test]
    fn a_power_cut_after_a_failed_open_keeps_each_header_whole() {
        let mut refused = Vec::new();
        let mut gave = 0_usize;
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let (mut log, _) = odd_open(&node).await.unwrap();
                log.write(None, &[bytes(1, 1710)]).await.unwrap();
                assert_eq!(log.offset, 1770);
                node.fail_file(&file("log-0"), Operation::Sync);
                let error = log.write(None, &[bytes(2, 700)]).await.unwrap_err();
                assert_eq!(error, io("log-0", Operation::Sync));
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, |node, _| async move {
                node.fail_file(&file("log-0"), Operation::Sync);
                let error = odd_open(&node).await.unwrap_err();
                assert_eq!(error, io("log-0", Operation::Sync));
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            let opened = sim
                .run_on(&node, |node, _| async move {
                    let opened = odd_open(&node).await;
                    opened.map(|(_, stored)| stored.entries.len())
                })
                .unwrap();
            if opened == Ok(2) {
                gave = gave.saturating_add(1);
            }
            if !matches!(opened, Ok(1 | 2)) {
                refused.push((run, opened));
            }
        }
        assert_eq!(
            refused,
            [],
            "(run, what the open after the power cuts gave)"
        );
        assert!(
            gave > 8,
            "runs in which the power cuts kept the record: {gave}"
        );
    }

    // The cache can drop the record of a failed sync between two reads of one sector.
    #[test]
    fn an_open_after_a_failed_sync_reads_each_header_whole() {
        let mut refused = Vec::new();
        let mut gave = 0_usize;
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            let opened = sim
                .run_on(&node, |node, _| async move {
                    let (mut log, _) = odd_open(&node).await.unwrap();
                    log.write(None, &[bytes(1, 1710)]).await.unwrap();
                    node.fail_file(&file("log-0"), Operation::Sync);
                    let error = log.write(None, &[bytes(2, 700)]).await.unwrap_err();
                    assert_eq!(error, io("log-0", Operation::Sync));
                    drop(log);
                    let opened = odd_open(&node).await;
                    opened.map(|(_, stored)| stored.entries.len())
                })
                .unwrap();
            if opened == Ok(2) {
                gave = gave.saturating_add(1);
            }
            if !matches!(opened, Ok(1 | 2)) {
                refused.push((run, opened));
            }
        }
        assert_eq!(
            refused,
            [],
            "(run, what the open after the failed sync gave)"
        );
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    // A pool of 512 bytes has a largest block of 448. A log that opens with it can
    // write a record that no later open can read.
    #[test]
    fn refuses_a_pool_with_no_block_of_one_sector() {
        for (budget, largest) in [(0, 0), (256, 192), (512, 448)] {
            let (mut sim, node) = create_node(0);
            let (new, listed, held) = sim
                .run_on(&node, move |node, _| async move {
                    let config = block::Config { budget };
                    let memory = block::Heap::new(config.reservation());
                    let pool = Rc::new(Pool::new(config, memory));
                    let files = node.files();
                    let dir = PathBuf::from(DIR);
                    let new = Log::open(files.clone(), dir.clone(), Rc::clone(&pool));
                    let new = new.await.err();
                    let listed = files.list(&dir).await.is_ok();
                    drop(open(&node).await.unwrap());
                    let held = Log::open(files, dir, pool).await.err();
                    (new, listed, held)
                })
                .unwrap();
            let requested = SECTOR;
            let cause = block::Error::TooLarge { requested, largest };
            let expected = Some(Error::Pool(cause));
            assert_eq!(new, expected, "budget {budget}, no log");
            assert!(!listed, "budget {budget}: the open made the directory");
            assert_eq!(held, expected, "budget {budget}, a log");
        }
    }

    // The record is 640 bytes, the largest block of a pool of 704 bytes. The pool has
    // no room for a block of one sector and a block of the rest.
    #[test]
    fn writes_a_record_that_one_block_of_the_pool_holds() {
        let (mut sim, node) = create_node(0);
        let written = sim
            .run_on(&node, |node, _| async move {
                let config = block::Config { budget: 704 };
                let memory = block::Heap::new(config.reservation());
                let pool = Rc::new(Pool::new(config, memory));
                let entry = bytes(1, 580);
                let record = encode(0, None, std::slice::from_ref(&entry)).len();
                assert_eq!((record, pool.largest()), (640, 640));
                let files = node.files();
                let (mut log, _) =
                    Log::open(files.clone(), DIR.into(), Rc::clone(&pool))
                        .await
                        .unwrap();
                let written = log.write(None, std::slice::from_ref(&entry)).await;
                drop(log);
                let (_, stored) = Log::open(files, DIR.into(), pool).await.unwrap();
                written.map(|()| stored.entries)
            })
            .unwrap();
        assert_eq!(written, Ok(vec![bytes(1, 580)]));
    }

    // The record is 1,892 bytes. The pool of 2,048 bytes holds it as blocks of 1,792
    // and 100 bytes, and has no block of 1,892.
    #[test]
    fn writes_a_record_that_no_block_of_the_pool_holds() {
        let (mut sim, node) = create_node(0);
        let written = sim
            .run_on(&node, |node, _| async move {
                let entry = bytes(1, 1832);
                let record = encode(0, None, std::slice::from_ref(&entry)).len();
                assert_eq!(record, 1892);
                let (mut log, _) = odd_open(&node).await.unwrap();
                let written = log.write(None, std::slice::from_ref(&entry)).await;
                drop(log);
                let (_, stored) = odd_open(&node).await.unwrap();
                written.map(|()| stored.entries)
            })
            .unwrap();
        assert_eq!(written, Ok(vec![bytes(1, 1832)]));
    }

    // With a block of 81,920 bytes held, the first pool has room for one block of
    // `CHUNK` and no more. The second has room for blocks of `CHUNK` and of 1 byte, and
    // for no second block of 81,920.
    #[test]
    fn a_block_of_a_write_is_at_most_a_chunk() {
        for (budget, record) in [(147_584, CHUNK), (147_712, CHUNK + 1)] {
            let (mut sim, node) = create_node(0);
            let written = sim
                .run_on(&node, move |node, _| async move {
                    let config = block::Config { budget };
                    let memory = block::Heap::new(config.reservation());
                    let pool = Rc::new(Pool::new(config, memory));
                    let entry = bytes(1, record - 60);
                    let encoded = encode(0, None, std::slice::from_ref(&entry));
                    assert_eq!(encoded.len(), record);
                    let files = node.files();
                    let (mut log, _) = Log::open(files, DIR.into(), Rc::clone(&pool))
                        .await
                        .unwrap();
                    let held = pool.alloc(81_920).unwrap();
                    let written = log.write(None, std::slice::from_ref(&entry)).await;
                    drop(held);
                    written
                })
                .unwrap();
            assert_eq!(written, Ok(()), "record {record}");
        }
    }

    // No pool here holds the blocks of its last record at one time. The first has no
    // room for blocks of `CHUNK` and of 16,384 bytes. The second has none for blocks of
    // 8,192 and of 1,025 bytes: the least block for 1,025 bytes is 1,280. The third
    // has none for the 16 blocks of its first record.
    #[test]
    fn a_write_holds_one_block_of_the_pool_at_a_time() {
        let cases: [(usize, &[usize]); 3] = [
            (82_000, &[81_920]),
            (9472, &[9217]),
            (1_000_000, &[984_040, 81_536]),
        ];
        for (budget, records) in cases {
            let (mut sim, node) = create_node(0);
            let entries: Vec<Entry> = records
                .iter()
                .zip(1..)
                .map(|(record, index)| bytes(index, record - 60))
                .collect();
            let expected = entries.clone();
            let written = sim
                .run_on(&node, move |node, _| async move {
                    let new = || {
                        let config = block::Config { budget };
                        let memory = block::Heap::new(config.reservation());
                        Rc::new(Pool::new(config, memory))
                    };
                    let files = node.files();
                    let (mut log, _) =
                        Log::open(files.clone(), DIR.into(), new()).await.unwrap();
                    let mut written = Vec::new();
                    for (entry, number) in entries.iter().zip(0..) {
                        let entry = std::slice::from_ref(entry);
                        written.push((
                            encode(number, None, entry).len(),
                            log.write(None, entry).await,
                        ));
                    }
                    drop(log);
                    let (_, stored) =
                        Log::open(files, DIR.into(), new()).await.unwrap();
                    (written, stored.entries)
                })
                .unwrap();
            let each = records.iter().map(|&record| (record, Ok(()))).collect();
            assert_eq!(written, (each, expected), "budget {budget}");
        }
    }

    #[test]
    fn opens_with_a_pool_whose_largest_block_is_one_sector() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let config = block::Config { budget: 576 };
            let memory = block::Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            assert_eq!(pool.largest(), SECTOR);
            let files = node.files();
            let (mut log, _) = Log::open(files.clone(), DIR.into(), Rc::clone(&pool))
                .await
                .unwrap();
            let entries = vec![bytes(1, 10), bytes(2, 400)];
            for entry in &entries {
                log.write(None, std::slice::from_ref(entry)).await.unwrap();
            }
            drop(log);
            let (_, stored) = Log::open(files, DIR.into(), pool).await.unwrap();
            assert_eq!(stored.entries, entries);
        })
        .unwrap();
    }

    #[test]
    fn each_open_gives_the_records_past_one_block() {
        let (mut sim, node) = create_node(0);
        let entries = vec![bytes(1, 10), bytes(2, 3 * CHUNK), bytes(3, 10)];
        let written = entries.clone();
        sim.run_on(&node, move |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            for entry in &written {
                log.write(None, std::slice::from_ref(entry)).await.unwrap();
            }
            assert_eq!(log.number, 0);
        })
        .unwrap();
        let expected = Stored {
            hard: Hard::default(),
            entries,
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected.clone()));
        assert_eq!(stored(&mut sim, &node), Ok(expected.clone()));
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn each_open_gives_the_records_of_three_files() {
        let (mut sim, node) = create_node(0);
        let expected = create_three_files(&mut sim, &node);
        assert_eq!(stored(&mut sim, &node), Ok(expected.clone()));
        assert_eq!(stored(&mut sim, &node), Ok(expected.clone()));
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    // A run, its entries before the power cut, and its entries after it.
    type Lost = (u64, usize, Result<usize, Error>);

    // Writes `before`, fails the sync of `failed` in the file `name`, opens the log
    // again, writes one more entry, and cuts the power. Gives each run that lost
    // what the open and the last write gave, and the count of runs in which the open
    // gave the record of the failed sync.
    fn lost_after_a_failed_sync(
        before: &[usize],
        failed: usize,
        name: &'static str,
    ) -> (Vec<Lost>, usize) {
        let mut lost = Vec::new();
        let mut gave = 0_usize;
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            let count = before.len();
            let before = before.to_vec();
            let expected = sim
                .run_on(&node, move |node, _| async move {
                    let (mut log, _) = open(&node).await.unwrap();
                    let count = wide(before.len());
                    for (len, index) in before.into_iter().zip(1..) {
                        log.write(None, &[bytes(index, len)]).await.unwrap();
                    }
                    node.fail_file(&file(name), Operation::Sync);
                    let entry = bytes(count.saturating_add(1), failed);
                    let error = log.write(None, &[entry]).await.unwrap_err();
                    assert_eq!(error, io(name, Operation::Sync));
                    drop(log);
                    let (mut log, mut opened) = open(&node).await.unwrap();
                    let held = wide(opened.entries.len());
                    let next = bytes(held.saturating_add(1), 100);
                    opened.entries.push(next.clone());
                    log.write(None, &[next]).await.unwrap();
                    opened
                })
                .unwrap();
            // The entries are those of `before`, the failed one or not, and the last.
            if expected.entries.len() > count.saturating_add(1) {
                gave = gave.saturating_add(1);
            }
            sim.crash(&node, Crash::Power);
            let kept = stored(&mut sim, &node);
            if kept != Ok(expected.clone()) {
                let kept = kept.map(|kept| kept.entries.len());
                lost.push((run, expected.entries.len(), kept));
            }
        }
        (lost, gave)
    }

    const LOST: &str = "(run, entries before the cut, entries after it)";
    const GAVE: &str = "runs in which the open gave the record of the failed sync";

    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_in_a_later_file() {
        let (lost, gave) = lost_after_a_failed_sync(&[10, LARGE, 700], 700, "log-2");
        assert_eq!(lost, [], "{LOST}");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_of_a_new_file() {
        let (lost, gave) = lost_after_a_failed_sync(&[10, LARGE], 700, "log-2");
        assert_eq!(lost, [], "{LOST}");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    // The record has 385 sectors, and no run keeps each of them in the cache: the
    // open finds a torn record, writes zeros on it, and the last record goes there.
    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_tore_a_record() {
        let (lost, _) = lost_after_a_failed_sync(&[100], 3 * CHUNK, "log-0");
        assert_eq!(lost, [], "{LOST}");
    }

    // The failed record starts in one block of the open's write and ends in the next.
    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_across_two_blocks() {
        let (lost, gave) = lost_after_a_failed_sync(&[CHUNK - 500], 700, "log-0");
        assert_eq!(lost, [], "{LOST}");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_in_the_fourth_block() {
        let (lost, gave) = lost_after_a_failed_sync(&[3 * CHUNK], 700, "log-0");
        assert_eq!(lost, [], "{LOST}");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    #[test]
    fn a_power_cut_keeps_what_an_open_gave_after_a_failed_sync_in_the_last_block() {
        let (lost, gave) = lost_after_a_failed_sync(&[15 * CHUNK], 700, "log-0");
        assert_eq!(lost, [], "{LOST}");
        assert!(gave > 16, "{GAVE}: {gave}");
    }

    #[test]
    fn a_record_over_a_torn_first_record_leaves_no_bytes_after_it() {
        for (torn, over) in [(700, 10), (3 * CHUNK, CHUNK + 100)] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, torn)]).await.unwrap();
                drop(log);
                put(&node, "log-0", wide(HEADER) + 50, &[0xFF]).await;
                let (mut log, opened) = open(&node).await.unwrap();
                assert_eq!(opened, Stored::default());
                log.write(None, &[bytes(1, over)]).await.unwrap();
            })
            .unwrap();
            let expected = Stored {
                hard: Hard::default(),
                entries: vec![bytes(1, over)],
            };
            assert_eq!(stored(&mut sim, &node), Ok(expected), "torn {torn}");
        }
    }

    // A power cut keeps a header whole or not at all, so a damaged one is never
    // the torn end.
    #[test]
    fn refuses_a_bad_header_at_the_end() {
        let (mut sim, node) = create_node(0);
        let starts = create_three(&mut sim, &node);
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
        let (mut sim, node) = create_node(0);
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
        for name in ["notes", "log-2", "log-01", "lock-1"] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                drop(open(&node).await.unwrap());
            })
            .unwrap();
            create_with_no_bytes(&mut sim, &node, &file(name));
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

    fn busy() -> Error {
        Error::Files(files::Error::Busy { path: file("lock") })
    }

    #[test]
    fn a_second_open_gives_busy_on_the_lock_while_the_first_log_lives() {
        for records in [0, 3] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                let (mut first, _) = open(&node).await.unwrap();
                for index in 1..=records {
                    first.write(None, &[bytes(index, 100)]).await.unwrap();
                }
                drop(first);
                let (first, _) = open(&node).await.unwrap();
                assert_eq!(open(&node).await.unwrap_err(), busy(), "{records}");
                let text = "file mesh/lock is open for writing in another handle";
                assert_eq!(busy().to_string(), text);
                drop(first);
                let (_, stored) = open(&node).await.unwrap();
                assert_eq!(wide(stored.entries.len()), records);
            })
            .unwrap();
        }
    }

    // The write starts `log-1`, so the log lets go of `log-0` while the write runs.
    #[test]
    fn a_second_open_gives_busy_on_the_lock_while_a_write_starts_a_file() {
        let (mut sim, node) = create_node(0);
        let entries = [bytes(1, 100), bytes(2, narrow(SEGMENT))];
        let written = entries.clone();
        sim.run_on(&node, move |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &written[..1]).await.unwrap();
            let mut polls = 0_u32;
            {
                let mut write = pin!(log.write(None, &written[1..]));
                while poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx)))
                    .await
                    .is_pending()
                {
                    assert_eq!(open(&node).await.unwrap_err(), busy(), "{polls}");
                    polls = polls.saturating_add(1);
                }
            }
            assert_ne!(polls, 0);
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            assert_eq!(names, ["lock", "log-0", "log-1"].map(PathBuf::from));
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: Hard::default(),
            entries: entries.into(),
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn an_open_that_meets_a_log_that_drops_gives_busy_or_each_entry() {
        let mut wrong = Vec::new();
        for steps in 0..32 {
            let (mut sim, node) = create_node(0);
            let result = sim
                .run_on(&node, move |node, _| async move {
                    let clock = node.clock();
                    let (mut first, _) = open(&node).await.unwrap();
                    first.write(None, &[bytes(1, 100)]).await.unwrap();
                    let mut second = pin!(open(&node));
                    for _ in 0..steps {
                        let poll =
                            poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx))).await;
                        if let Poll::Ready(result) = poll {
                            return result.map(|(_, stored)| stored.entries.len());
                        }
                        clock.sleep(Span::from_nanos(20_000)).await;
                    }
                    first
                        .write(None, &[bytes(2, narrow(SEGMENT))])
                        .await
                        .unwrap();
                    drop(first);
                    second.await.map(|(_, stored)| stored.entries.len())
                })
                .unwrap();
            if result != Err(busy()) && result != Ok(2) {
                wrong.push((steps, result));
            }
        }
        assert_eq!(wrong, []);
    }

    #[test]
    fn a_power_cut_frees_the_lock_of_a_log_that_lives() {
        let (mut sim, node) = create_node(0);
        let own = node.clone();
        let handle = node.shards().start(shard("holder"), move |_| async move {
            let (mut log, _) = open(&own).await.unwrap();
            log.write(None, &[bytes(1, 100)]).await.unwrap();
            pending::<()>().await;
        });
        drop(handle.unwrap());
        sim.run_for(Span::SECOND).unwrap();
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 100)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn opens_a_directory_that_holds_only_the_lock() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let files = node.files();
            files.create_dir(Path::new(DIR)).await.unwrap();
            let mode = Mode::Create { len: 0 };
            drop(files.open(&file("lock"), mode).await.unwrap());
            assert_eq!(files.list(Path::new(DIR)).await, Ok(vec!["lock".into()]));
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(Stored::default()));
    }

    #[test]
    fn refuses_a_lock_that_has_bytes() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let files = node.files();
            files.create_dir(Path::new(DIR)).await.unwrap();
            let mode = Mode::Create { len: wide(SECTOR) };
            drop(files.open(&file("lock"), mode).await.unwrap());
        })
        .unwrap();
        let error = stored(&mut sim, &node).unwrap_err();
        let length = files::Error::Length {
            path: file("lock"),
            expected: 0,
            found: wide(SECTOR),
        };
        assert_eq!(error, Error::Files(length));
        let text = "file mesh/lock has 512 bytes, but 0 bytes were expected";
        assert_eq!(error.to_string(), text);
    }

    #[test]
    fn refuses_a_bad_record_with_a_good_one_after_it() {
        let (mut sim, node) = create_node(0);
        let starts = create_three(&mut sim, &node);
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

    #[test]
    fn refuses_a_record_that_passes_its_check_with_a_body_it_cannot_read() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 4)]);
            // The kind of the entry.
            record[HEADER + 17] = 9;
            seal(&mut record);
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
    fn refuses_a_hard_record_with_a_byte_it_cannot_read() {
        // The presence byte of the vote, then the grant byte of the proof.
        for at in [HEADER + 9, HEADER + 44] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                drop(open(&node).await.unwrap());
                let hard = Hard {
                    leader: Some(key(2)),
                    proof: Some(proof(Grant::Vote, 2, &[2, 3])),
                    ..hard(5, Some(2))
                };
                let mut record = encode(0, Some(hard), &[]);
                record[at] = 2;
                seal(&mut record);
                put(&node, "log-0", 0, &record).await;
            })
            .unwrap();
            let expected = Error::Corrupt {
                path: file("log-0"),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "byte {at}");
        }
    }

    #[test]
    fn a_hard_record_has_one_byte_form() {
        let hard = Hard {
            leader: Some(key(2)),
            proof: Some(proof(Grant::Vote, 2, &[2, 3])),
            ..hard(5, Some(2))
        };
        let record = encode(0, Some(hard), &[]);
        let two = 2_u128.to_le_bytes();
        let mut expected = vec![HARD];
        expected.extend(5_u64.to_le_bytes());
        expected.extend([PRESENT].into_iter().chain(two));
        expected.extend([PRESENT].into_iter().chain(two));
        expected.extend([PRESENT, VOTE].into_iter().chain(two));
        expected.extend(2_u64.to_le_bytes());
        expected.extend(two.into_iter().chain([2; 64]));
        expected.extend(3_u128.to_le_bytes().into_iter().chain([3; 64]));
        assert_eq!(record[HEADER..], expected);
    }

    #[test]
    #[should_panic(expected = "invariant: a claim is signed before it is encoded")]
    fn encode_panics_on_an_unsigned_proof_entry() {
        let mut proof = proof(Grant::Vote, 2, &[2, 3]);
        proof.voters.insert(key(2), None);
        let hard = Hard {
            proof: Some(proof),
            ..hard(5, Some(2))
        };
        encode(0, Some(hard), &[]);
    }

    #[test]
    fn refuses_a_record_of_another_format_version() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, Some(hard(1, None)), &[]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            seal(&mut record);
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

    #[test]
    fn gives_the_version_error_for_a_record_of_another_version_past_the_end() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 10)]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            let past = (2 * SEGMENT).to_le_bytes();
            record[CHECK + 10..CHECK + 18].copy_from_slice(&past);
            // Not `seal`, which writes the true length.
            let head = check(&record[CHECK..HEADER]);
            record[..CHECK].copy_from_slice(&head);
            put(&node, "log-0", 0, &record).await;
        })
        .unwrap();
        let expected = Error::Version {
            path: file("log-0"),
            found: 2,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn gives_the_version_error_for_a_torn_record_of_another_version() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 10)]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            let last = record.len() - 1;
            record[last] ^= 1;
            let head = check(&record[CHECK..HEADER]);
            record[..CHECK].copy_from_slice(&head);
            put(&node, "log-0", 0, &record).await;
        })
        .unwrap();
        let expected = Error::Version {
            path: file("log-0"),
            found: 2,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    const LARGE: usize = 3 << 19;

    /// Writes a small record, one larger than a file, and two small ones: three files.
    fn create_three_files(sim: &mut Sim, node: &sim::node::Node) -> Stored {
        sim.run_on(node, |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            let entries =
                vec![bytes(1, 10), bytes(2, LARGE), bytes(3, 10), bytes(4, 10)];
            for entry in &entries {
                log.write(None, std::slice::from_ref(entry)).await.unwrap();
            }
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            let expected = ["lock", "log-0", "log-1", "log-2"].map(PathBuf::from);
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
        let (mut sim, node) = create_node(0);
        let expected = create_three_files(&mut sim, &node);
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn a_record_that_ends_at_the_end_of_a_file_stays_in_it() {
        let (mut sim, node) = create_node(0);
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
        assert_eq!(names, ["lock", "log-0"].map(PathBuf::from));
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 10), bytes(2, len)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn refuses_a_file_that_starts_with_a_record_before_the_next_one() {
        let (mut sim, node) = create_node(0);
        create_three_files(&mut sim, &node);
        sim.run_on(&node, |node, _| async move {
            let stale = encode(0, None, &[bytes(1, 10)]);
            put(&node, "log-1", 0, &stale).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_torn_record_after_the_last_that_is_not_the_next_one() {
        let first = encode(0, None, &[bytes(1, 10)]);
        let offset = wide(first.len());
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut torn = encode(2, None, &[bytes(2, 10)]);
            *torn.last_mut().unwrap() ^= 1;
            put(&node, "log-0", 0, &[first, torn].concat()).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_bad_last_record_of_a_file_before_a_file_with_records() {
        for (name, number) in [("log-0", 0), ("log-1", 1)] {
            let (mut sim, node) = create_node(0);
            create_three_files(&mut sim, &node);
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

    // The number 9 of the next file is after the end. A number of another version has
    // no meaning to this build.
    #[test]
    fn gives_the_version_error_for_a_next_file_of_another_version() {
        let (mut sim, node) = create_node(0);
        create_three_files(&mut sim, &node);
        put_other_version(&mut sim, &node);
        let expected = Error::Version {
            path: file("log-2"),
            found: 2,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_next_file_of_another_version_after_a_torn_record() {
        let (mut sim, node) = create_node(0);
        create_three_files(&mut sim, &node);
        put_other_version(&mut sim, &node);
        sim.run_on(&node, |node, _| async move {
            put(&node, "log-1", wide(HEADER) + 5, &[0xFF]).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    /// Writes a record with number 9 and format version 2 at the start of `log-2`.
    fn put_other_version(sim: &mut Sim, node: &sim::node::Node) {
        sim.run_on(node, |node, _| async move {
            let mut record = encode(9, None, &[bytes(4, 10)]);
            record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            seal(&mut record);
            put(&node, "log-2", 0, &record).await;
        })
        .unwrap();
    }

    // A record right after a torn one, or at the start of the next file, was written
    // after the torn one, whatever its version or number.
    #[test]
    fn refuses_a_record_of_another_version_after_a_torn_one_in_its_file() {
        let (mut sim, node) = create_node(0);
        create_three_files(&mut sim, &node);
        sim.run_on(&node, |node, _| async move {
            let mut torn = encode(2, None, &[bytes(3, 10)]);
            let last = torn.len() - 1;
            torn[last] ^= 0xFF;
            let mut other = encode(3, None, &[bytes(4, 10)]);
            other[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            seal(&mut other);
            put(&node, "log-2", 0, &[torn, other].concat()).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-2"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_a_next_file_that_starts_at_or_before_a_torn_record() {
        for number in [0, 1] {
            let (mut sim, node) = create_node(0);
            create_three_files(&mut sim, &node);
            sim.run_on(&node, move |node, _| async move {
                put(&node, "log-1", wide(HEADER) + 5, &[0xFF]).await;
                let record = encode(number, None, &[bytes(3, 10)]);
                put(&node, "log-2", 0, &record).await;
            })
            .unwrap();
            let expected = Error::Corrupt {
                path: file("log-1"),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "next at {number}");
        }
    }

    #[test]
    fn a_torn_record_before_a_spare_is_the_end() {
        let (mut sim, node) = create_node(0);
        create_spare(&mut sim, &node, 512);
        sim.run_on(&node, |node, _| async move {
            put(&node, "log-0", wide(HEADER) + 5, &[0xFF]).await;
            let (_, stored) = open(&node).await.unwrap();
            assert_eq!(stored, Stored::default());
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            assert_eq!(names, ["lock", "log-0"].map(PathBuf::from));
        })
        .unwrap();
    }

    #[test]
    fn refuses_garbage_in_a_file_after_a_torn_record() {
        let (mut sim, node) = create_node(0);
        create_spare(&mut sim, &node, 512);
        sim.run_on(&node, |node, _| async move {
            put(&node, "log-0", wide(HEADER) + 5, &[0xFF]).await;
            put(&node, "log-1", 0, &[0xAB; HEADER]).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    // A crash can leave the next file with no record. Its length can differ from
    // the one the next record needs.
    #[test]
    fn removes_a_file_with_no_record_after_the_end() {
        removes_a_spare(512);
    }

    // A crash before the allocation of a create leaves a file with no bytes.
    #[test]
    fn removes_a_file_with_no_bytes_after_the_end() {
        removes_a_spare(0);
    }

    /// Writes one record, with entry 1, to `log-0` and makes `log-1` with `len` bytes
    /// and no record, then cuts the power. With no bytes, the cut is in its create.
    fn create_spare(sim: &mut Sim, node: &sim::node::Node, len: u64) {
        sim.run_on(node, move |node, _| async move {
            let (mut log, _) = open(&node).await.unwrap();
            log.write(None, &[bytes(1, 10)]).await.unwrap();
            if len > 0 {
                let mode = Mode::Create { len };
                drop(node.files().open(&file("log-1"), mode).await.unwrap());
            }
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
        })
        .unwrap();
        if len == 0 {
            create_with_no_bytes(sim, node, &file("log-1"));
        } else {
            sim.crash(node, Crash::Power);
        }
    }

    /// The files of a log after `create_spare`.
    fn spare(len: u64) -> [(PathBuf, u64); 3] {
        [
            ("lock".into(), 0),
            ("log-0".into(), SEGMENT),
            ("log-1".into(), len),
        ]
    }

    fn removes_a_spare(len: u64) {
        let (mut sim, node) = create_node(0);
        create_spare(&mut sim, &node, len);
        sim.run_on(&node, move |node, _| async move {
            assert_eq!(lens(&node).await, spare(len));
            let (mut log, stored) = open(&node).await.unwrap();
            assert_eq!(stored.entries, [bytes(1, 10)]);
            let names = node.files().list(Path::new(DIR)).await.unwrap();
            assert_eq!(names, ["lock", "log-0"].map(PathBuf::from));
            log.write(None, &[bytes(2, LARGE)]).await.unwrap();
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 10), bytes(2, LARGE)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    #[test]
    fn a_failed_remove_of_a_spare_fails_the_open() {
        for len in [0, 512] {
            let (mut sim, node) = create_node(0);
            create_spare(&mut sim, &node, len);
            let (refused, lens) = sim
                .run_on(&node, |node, _| async move {
                    node.fail_file(&file("log-1"), Operation::Remove);
                    let refused = open(&node).await.map(|(_, stored)| stored);
                    (refused, lens(&node).await)
                })
                .unwrap();
            let expected = io("log-1", Operation::Remove);
            assert_eq!(refused, Err(expected), "{len} bytes");
            assert_eq!(lens, spare(len));
            sim.crash(&node, Crash::Power);
            let expected = Stored {
                hard: Hard::default(),
                entries: vec![bytes(1, 10)],
            };
            assert_eq!(stored(&mut sim, &node), Ok(expected), "{len} bytes");
        }
    }

    fn named((name, len): (&str, u64)) -> (PathBuf, u64) {
        (name.into(), len)
    }

    /// The name and the length of each file of the log.
    async fn lens(node: &sim::node::Node) -> Vec<(PathBuf, u64)> {
        let mut lens = Vec::new();
        for name in node.files().list(Path::new(DIR)).await.unwrap() {
            let path = Path::new(DIR).join(&name);
            let len = node.files().open(&path, Mode::Read).await.unwrap().len();
            lens.push((name, len));
        }
        lens
    }

    /// Makes the file at `path` with no bytes through power crashes in its create.
    /// The directory of `path` must be durable.
    fn create_with_no_bytes(sim: &mut Sim, node: &sim::node::Node, path: &Path) {
        for _ in 0..64 {
            let (own, cut) = (node.clone(), path.to_owned());
            let handle = node.shards().start(shard("create"), move |_| async move {
                let mode = Mode::Create { len: SEGMENT };
                drop(own.files().open(&cut, mode).await);
                pending::<()>().await;
            });
            drop(handle.unwrap());
            sim.run_for(Span::from_nanos(1_000)).unwrap();
            sim.crash(node, Crash::Power);
            let own = path.to_owned();
            let len = sim
                .run_on(node, move |node, _| async move {
                    let opened = node.files().open(&own, Mode::Read).await;
                    opened.ok().map(|file| file.len())
                })
                .unwrap();
            match len {
                Some(0) => return,
                Some(_) => {
                    let whole = path.to_owned();
                    sim.run_on(node, move |node, _| async move {
                        node.files().remove(&whole).await.unwrap();
                        let dir = whole.parent().expect("a file in a directory");
                        node.files().sync_dir(dir).await.unwrap();
                    })
                    .unwrap();
                }
                None => {}
            }
        }
        panic!("64 power crashes in a create left no {}", path.display());
    }

    /// Makes `log-0` with no bytes through power crashes in its create.
    fn create_first_file_with_no_bytes(sim: &mut Sim, node: &sim::node::Node) {
        sim.run_on(node, |node, _| async move {
            node.files().create_dir(Path::new(DIR)).await.unwrap();
            node.files().sync_dir(Path::new("")).await.unwrap();
        })
        .unwrap();
        create_with_no_bytes(sim, node, &file("log-0"));
    }

    #[test]
    fn a_first_file_with_no_bytes_is_an_empty_log() {
        let (mut sim, node) = create_node(0);
        create_first_file_with_no_bytes(&mut sim, &node);
        sim.run_on(&node, |node, _| async move {
            assert_eq!(lens(&node).await, [(PathBuf::from("log-0"), 0)]);
            let (mut log, stored) = open(&node).await.unwrap();
            assert_eq!(stored, Stored::default());
            log.write(None, &[bytes(1, 10)]).await.unwrap();
            drop(log);
            let expected = [("lock", 0), ("log-0", SEGMENT)];
            assert_eq!(lens(&node).await, expected.map(named));
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let expected = Stored {
            hard: Hard::default(),
            entries: vec![bytes(1, 10)],
        };
        assert_eq!(stored(&mut sim, &node), Ok(expected));
    }

    // The write removes a first file with no record to make it again, larger.
    #[test]
    fn a_failed_remove_of_a_first_file_with_no_bytes_poisons_the_log() {
        let (mut sim, node) = create_node(0);
        create_first_file_with_no_bytes(&mut sim, &node);
        let (errors, lens) = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                node.fail_file(&file("log-0"), Operation::Remove);
                let first = log.write(None, &[bytes(1, 10)]).await.unwrap_err();
                let second = log.write(None, &[bytes(1, 10)]).await.unwrap_err();
                drop(log);
                ((first, second), lens(&node).await)
            })
            .unwrap();
        let poisoned = Error::Poisoned {
            path: file("log-0"),
        };
        assert_eq!(errors, (io("log-0", Operation::Remove), poisoned));
        assert_eq!(lens, [("lock", 0), ("log-0", 0)].map(named));
        sim.crash(&node, Crash::Power);
        assert_eq!(stored(&mut sim, &node), Ok(Stored::default()));
    }

    #[test]
    fn gives_the_error_of_a_file_call_that_fails() {
        let (mut sim, node) = create_node(0);
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
        let (mut sim, node) = create_node(0);
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
            let (mut sim, node) = create_node(0);
            let starts = create_three(&mut sim, &node);
            sim.run_on(&node, |node, _| async move {
                let (mut log, stored) = open(&node).await.unwrap();
                assert_eq!(stored.entries.len(), 3);
                log.write(None, &[bytes(4, LARGE)]).await.unwrap();
                let names = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(names, ["lock", "log-0", "log-1"].map(PathBuf::from));
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
            let kept = ["lock", "log-0", "log-1"].map(PathBuf::from).to_vec();
            assert_eq!((result, names), (Err(expected), kept), "record {record}");
        }
    }

    #[test]
    fn refuses_a_file_with_no_record_before_another_file() {
        let (mut sim, node) = create_node(0);
        create_three(&mut sim, &node);
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

    // Record 0 in `log-1` is the record that `log-0` needs, so no number is wrong.
    #[test]
    fn refuses_a_first_file_with_no_record_before_a_file_with_records() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mode = Mode::Create { len: SEGMENT };
            drop(node.files().open(&file("log-1"), mode).await.unwrap());
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
            put(&node, "log-1", 0, &encode(0, None, &[bytes(1, 10)])).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    /// Writes a record with `number` and format version 2 at the start of `name`.
    async fn put_version_2(node: &sim::node::Node, name: &str, number: u64) {
        let mut record = encode(number, None, &[bytes(2, 10)]);
        record[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
        seal(&mut record);
        put(node, name, 0, &record).await;
    }

    #[test]
    fn refuses_an_empty_log_0_before_garbage_or_another_version() {
        for garbage in [false, true] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                drop(open(&node).await.unwrap());
                let mode = Mode::Create { len: SEGMENT };
                drop(node.files().open(&file("log-1"), mode).await.unwrap());
                node.files().sync_dir(Path::new(DIR)).await.unwrap();
                if garbage {
                    put(&node, "log-1", 0, &[0xAB; 40]).await;
                } else {
                    put_version_2(&node, "log-1", 0).await;
                }
            })
            .unwrap();
            let expected = Error::Corrupt {
                path: file("log-0"),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "garbage {garbage}");
        }
    }

    #[test]
    fn opens_an_empty_log_0_before_an_empty_last_file() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mode = Mode::Create { len: SEGMENT };
            drop(node.files().open(&file("log-1"), mode).await.unwrap());
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(Stored::default()));
    }

    // Bytes too few for a header that are not zeros are not the end of the log.
    #[test]
    fn refuses_a_short_file_that_is_not_zeros() {
        // One byte that is not zero, at each place.
        let shorts = (0..10).map(|at| {
            let mut short = [0_u8; 10];
            short[at] = 0xAB;
            short
        });
        // `log-0` with no record, then with record 0.
        let cases = [(0, "log-0"), (1, "log-1")];
        for ((records, blamed), short) in cases
            .into_iter()
            .flat_map(|case| shorts.clone().map(move |short| (case, short)))
        {
            let (mut sim, node) = create_node(0);
            let names = sim
                .run_on(&node, move |node, _| async move {
                    drop(open(&node).await.unwrap());
                    if records == 1 {
                        let record = encode(0, None, &[bytes(1, 10)]);
                        put(&node, "log-0", 0, &record).await;
                    } else {
                        node.files().remove(&file("log-0")).await.unwrap();
                        let mode = Mode::Create { len: 0 };
                        drop(node.files().open(&file("log-0"), mode).await.unwrap());
                    }
                    let mode = Mode::Create { len: 10 };
                    drop(node.files().open(&file("log-1"), mode).await.unwrap());
                    node.files().sync_dir(Path::new(DIR)).await.unwrap();
                    put(&node, "log-1", 0, &short).await;
                    lens(&node).await
                })
                .unwrap();
            let case = format!("records {records}, {short:?}");
            assert_eq!(names[2], named(("log-1", 10)), "{case}");
            let expected = Error::Corrupt {
                path: file(blamed),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "{case}");
        }
    }

    #[test]
    fn gives_the_version_error_for_a_record_of_another_version_past_next() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            put(&node, "log-0", 0, &encode(0, None, &[bytes(1, 10)])).await;
            let mode = Mode::Create { len: SEGMENT };
            drop(node.files().open(&file("log-1"), mode).await.unwrap());
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
            put_version_2(&node, "log-1", 5).await;
        })
        .unwrap();
        let expected = Error::Version {
            path: file("log-1"),
            found: 2,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    // The first empty file is the defect.
    #[test]
    fn refuses_an_empty_log_1_before_a_record_of_another_version() {
        for (records, blamed) in [(0, "log-0"), (1, "log-1")] {
            let (mut sim, node) = create_node(0);
            sim.run_on(&node, move |node, _| async move {
                drop(open(&node).await.unwrap());
                if records == 1 {
                    put(&node, "log-0", 0, &encode(0, None, &[bytes(1, 10)])).await;
                }
                for name in ["log-1", "log-2"] {
                    let mode = Mode::Create { len: SEGMENT };
                    drop(node.files().open(&file(name), mode).await.unwrap());
                }
                node.files().sync_dir(Path::new(DIR)).await.unwrap();
                put_version_2(&node, "log-2", records).await;
            })
            .unwrap();
            let expected = Error::Corrupt {
                path: file(blamed),
                offset: 0,
            };
            assert_eq!(stored(&mut sim, &node), Err(expected), "records {records}");
        }
    }

    #[test]
    fn refuses_an_empty_file_after_a_torn_end_before_another_file() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 10)]);
            *record.last_mut().unwrap() ^= 0xFF;
            put(&node, "log-0", 0, &record).await;
            for name in ["log-1", "log-2"] {
                let mode = Mode::Create { len: SEGMENT };
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

    #[test]
    fn refuses_an_empty_file_before_garbage_and_another_version() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            put(&node, "log-0", 0, &encode(0, None, &[bytes(1, 10)])).await;
            for name in ["log-1", "log-2", "log-3"] {
                let mode = Mode::Create { len: SEGMENT };
                drop(node.files().open(&file(name), mode).await.unwrap());
            }
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
            put(&node, "log-2", 0, &[0xAB; 40]).await;
            let mut other = encode(1, None, &[bytes(2, 10)]);
            other[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            seal(&mut other);
            put(&node, "log-3", 0, &other).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_an_empty_file_after_a_torn_end_before_another_version() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            let mut record = encode(0, None, &[bytes(1, 10)]);
            *record.last_mut().unwrap() ^= 0xFF;
            put(&node, "log-0", 0, &record).await;
            for name in ["log-1", "log-2"] {
                let mode = Mode::Create { len: SEGMENT };
                drop(node.files().open(&file(name), mode).await.unwrap());
            }
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
            let mut other = encode(0, None, &[bytes(2, 10)]);
            other[CHECK..CHECK + 2].copy_from_slice(&2_u16.to_le_bytes());
            seal(&mut other);
            put(&node, "log-2", 0, &other).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-1"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    #[test]
    fn refuses_an_empty_log_0_before_an_empty_log_1_and_a_record() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            drop(open(&node).await.unwrap());
            for name in ["log-1", "log-2"] {
                let mode = Mode::Create { len: SEGMENT };
                drop(node.files().open(&file(name), mode).await.unwrap());
            }
            node.files().sync_dir(Path::new(DIR)).await.unwrap();
            put(&node, "log-2", 0, &encode(0, None, &[bytes(1, 10)])).await;
        })
        .unwrap();
        let expected = Error::Corrupt {
            path: file("log-0"),
            offset: 0,
        };
        assert_eq!(stored(&mut sim, &node), Err(expected));
    }

    // A cut can keep the header of the first record of a file and tear its body.
    #[test]
    fn a_record_that_does_not_fit_replaces_a_file_with_no_record() {
        let (mut sim, node) = create_node(0);
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
                assert_eq!(names, ["lock", "log-0", "log-1"].map(PathBuf::from));
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
        let (mut sim, node) = create_node(0);
        let files = sim
            .run_on(&node, |node, _| async move {
                let (mut log, _) = open(&node).await.unwrap();
                log.write(None, &[bytes(1, LARGE)]).await.unwrap();
                drop(log);
                lens(&node).await
            })
            .unwrap();
        let len = wide(encode(0, None, &[bytes(1, LARGE)]).len());
        assert_eq!(files, [("lock", 0), ("log-0", len)].map(named));
    }

    #[test]
    fn refuses_a_write_after_a_failed_or_dropped_one() {
        let (mut sim, node) = create_node(0);
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

    /// A log on a pool of 2,048 bytes, with one record to byte 1,236 and the block
    /// at the end of a second record at bytes 1,536 to 1,576: the pool gave no block
    /// for the 300 bytes before it. Returns the log, its pool, and the block that keeps
    /// the pool full.
    async fn stopped(node: &sim::node::Node) -> (Log, Rc<Pool>, block::Unique) {
        let pool = odd_pool();
        let (mut log, _) = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
            .await
            .unwrap();
        log.write(None, &[bytes(1, 1176)]).await.unwrap();
        let held = pool.alloc(1792).unwrap();
        let error = log.write(None, &[bytes(2, 280)]).await.unwrap_err();
        assert_eq!(error, Error::Pool(exhausted(300, 64)));
        (log, pool, held)
    }

    fn exhausted(requested: usize, available: usize) -> block::Error {
        block::Error::Exhausted {
            requested,
            available,
        }
    }

    fn records(lens: &[usize]) -> Stored {
        Stored {
            hard: Hard::default(),
            entries: lens
                .iter()
                .zip(1..)
                .map(|(&len, index)| bytes(index, len))
                .collect(),
        }
    }

    #[test]
    fn a_write_that_the_pool_stops_leaves_a_log_that_takes_a_write() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, pool, held) = stopped(&node).await;
            let again = log.write(None, &[bytes(2, 280)]).await.unwrap_err();
            assert_eq!(again, Error::Pool(pool.alloc(300).unwrap_err()));
            drop(held);
            log.write(None, &[bytes(2, 280)]).await.unwrap();
            log.write(None, &[bytes(3, 10)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[1176, 280, 10])));
    }

    // The body of the record is zeros, as the file is after byte 400. A write that
    // puts the block with the header first leaves a whole record.
    #[test]
    fn a_stopped_write_of_a_record_that_ends_in_zeros_holds_what_it_held() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let pool = odd_pool();
            let (mut log, _) = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                .await
                .unwrap();
            log.write(None, &[bytes(1, 340)]).await.unwrap();
            let _held = pool.alloc(448).unwrap();
            let zeros = entry(1, 2, Data::Bytes(vec![0; 3000]));
            let error = log.write(None, &[zeros]).await.unwrap_err();
            assert_eq!(error, Error::Pool(exhausted(3 * SECTOR, 1024)));
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[340])));
    }

    // The record ends at byte 1,536, so the header after it starts in the sector that
    // holds the bytes of the stopped write.
    #[test]
    fn a_shorter_record_after_a_stopped_write_has_no_bytes_after_it() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, _, held) = stopped(&node).await;
            drop(held);
            log.write(None, &[bytes(2, 240)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[1176, 240])));
    }

    // The record ends one byte before the end of the stopped write.
    #[test]
    fn the_zeros_reach_the_end_of_the_stopped_bytes() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, _, held) = stopped(&node).await;
            drop(held);
            log.write(None, &[bytes(2, 279)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[1176, 279])));
    }

    // The pool has no block for the first zeros, which go over the bytes of the
    // stopped write.
    #[test]
    fn a_pool_that_stops_the_zeros_keeps_the_stopped_bytes_known() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let (mut log, pool, held) = stopped(&node).await;
            let other = pool.alloc(40).unwrap();
            let error = log.write(None, &[bytes(2, 279)]).await.unwrap_err();
            assert_eq!(error, Error::Pool(exhausted(40, 64)));
            drop((held, other));
            log.write(None, &[bytes(2, 279)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[1176, 279])));
    }

    // The stopped write put two blocks, at bytes 131,072 and 65,536. The record after
    // it ends at byte 65,536, so a header there is in the bytes of the second block.
    #[test]
    fn the_zeros_cover_each_block_of_the_stopped_write() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let config = block::Config { budget: 140_000 };
            let memory = block::Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            let (mut log, _) = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                .await
                .unwrap();
            log.write(None, &[bytes(1, 10_000)]).await.unwrap();
            let held = pool.alloc(CHUNK).unwrap();
            let error = log.write(None, &[bytes(2, 120_992)]).await.unwrap_err();
            assert_eq!(error, Error::Pool(exhausted(55_476, 8672)));
            drop(held);
            log.write(None, &[bytes(2, 55_416)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[10_000, 55_416])));
    }

    // The pool gives no block for the end of the record, so the write puts no byte. The
    // sync that fails is the one of the next record, which the files keep or not. If
    // the log synced zeros first, that sync fails in its place, and the log has no
    // second record.
    #[test]
    fn a_stopped_write_that_put_no_block_needs_no_zeros() {
        let mut kept = 0;
        for run in 0..8 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let pool = odd_pool();
                let files = node.files();
                let (mut log, _) = Log::open(files, DIR.into(), Rc::clone(&pool))
                    .await
                    .unwrap();
                log.write(None, &[bytes(1, 340)]).await.unwrap();
                let _held = pool.alloc(1792).unwrap();
                let error = log.write(None, &[bytes(2, 3000)]).await.unwrap_err();
                assert_eq!(error, Error::Pool(exhausted(388, 192)));
                node.fail_file(&file("log-0"), Operation::Sync);
                let error = log.write(None, &[bytes(2, 10)]).await.unwrap_err();
                assert_eq!(error, io("log-0", Operation::Sync));
            })
            .unwrap();
            let stored = stored(&mut sim, &node);
            if stored == Ok(records(&[340, 10])) {
                kept += 1;
            } else {
                assert_eq!(stored, Ok(records(&[340])), "run {run}");
            }
        }
        assert!(kept > 0);
    }

    // The stopped write put one block of 100 bytes, at byte 3,072, and the pool has
    // no block for zeros over the 2,672 bytes before it.
    #[test]
    fn the_zeros_cover_no_byte_that_the_stopped_write_did_not_put() {
        let (mut sim, node) = create_node(0);
        sim.run_on(&node, |node, _| async move {
            let pool = odd_pool();
            let (mut log, _) = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                .await
                .unwrap();
            log.write(None, &[bytes(1, 340)]).await.unwrap();
            let _held = pool.alloc(1792).unwrap();
            let error = log.write(None, &[bytes(2, 2712)]).await.unwrap_err();
            assert_eq!(error, Error::Pool(exhausted(1536, 0)));
            log.write(None, &[bytes(2, 10)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[340, 10])));
    }

    /// Writes a record for each of `before`, then a record of 3 MiB that the pool
    /// stops after one block, then a record of 100 bytes. Returns the name and the
    /// length of each file.
    fn files_after_a_stopped_large_write(
        before: &'static [usize],
    ) -> Vec<(PathBuf, u64)> {
        let (mut sim, node) = create_node(0);
        let next = before.len().saturating_add(1);
        let files = sim
            .run_on(&node, move |node, _| async move {
                let pool = odd_pool();
                let (mut log, _) =
                    Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                        .await
                        .unwrap();
                for entry in records(before).entries {
                    log.write(None, &[entry]).await.unwrap();
                }
                let held = pool.alloc(1792).unwrap();
                let large = bytes(wide(next), 3 << 20);
                let error = log.write(None, &[large]).await.unwrap_err();
                assert_eq!(error, Error::Pool(exhausted(1536, 64)));
                drop(held);
                log.write(None, &[bytes(wide(next), 100)]).await.unwrap();
                drop(log);
                lens(&node).await
            })
            .unwrap();
        sim.crash(&node, Crash::Power);
        let lens: Vec<usize> = before.iter().copied().chain([100]).collect();
        assert_eq!(stored(&mut sim, &node), Ok(records(&lens)));
        files
    }

    // The log made `log-0` again for the record of 3 MiB, which is 3,145,788 bytes
    // with its header.
    #[test]
    fn a_record_after_a_stopped_first_record_starts_the_file_of_that_record() {
        let files = files_after_a_stopped_large_write(&[]);
        assert_eq!(files, [("lock", 0), ("log-0", 3_145_788)].map(named));
    }

    // The record of 100 bytes fits in `log-0`, and the end of the log is in `log-1`.
    #[test]
    fn a_record_after_a_stopped_record_starts_the_file_of_that_record() {
        let files = files_after_a_stopped_large_write(&[10]);
        let expected = [("lock", 0), ("log-0", SEGMENT), ("log-1", 3_145_788)];
        assert_eq!(files, expected.map(named));
    }

    // The sync that fails is the one of the third record, which the files keep or
    // not. If the log synced the zeros a second time, that sync fails in its place,
    // and the log has no third record.
    #[test]
    fn the_zeros_go_in_one_time() {
        let mut kept = 0;
        for run in 0..8 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let (mut log, _, held) = stopped(&node).await;
                drop(held);
                log.write(None, &[bytes(2, 280)]).await.unwrap();
                node.fail_file(&file("log-0"), Operation::Sync);
                let error = log.write(None, &[bytes(3, 10)]).await.unwrap_err();
                assert_eq!(error, io("log-0", Operation::Sync));
            })
            .unwrap();
            let stored = stored(&mut sim, &node);
            if stored == Ok(records(&[1176, 280, 10])) {
                kept += 1;
            } else {
                assert_eq!(stored, Ok(records(&[1176, 280])), "run {run}");
            }
        }
        assert!(kept > 0);
    }

    #[test]
    fn a_write_after_a_stopped_write_can_start_a_new_file() {
        let (mut sim, node) = create_node(0);
        let len = narrow(SEGMENT);
        sim.run_on(&node, move |node, _| async move {
            let (mut log, _, held) = stopped(&node).await;
            drop(held);
            log.write(None, &[bytes(2, len)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[1176, len])));
    }

    // The pool stops the first record of a new file after the block at its end. The
    // record after it starts that file, and ends in the bytes of the stopped write.
    #[test]
    fn a_write_after_the_pool_stops_a_new_file_starts_that_file() {
        let (mut sim, node) = create_node(0);
        let len = narrow(SEGMENT);
        sim.run_on(&node, move |node, _| async move {
            let pool = odd_pool();
            let (mut log, _) = Log::open(node.files(), DIR.into(), Rc::clone(&pool))
                .await
                .unwrap();
            log.write(None, &[bytes(1, 340)]).await.unwrap();
            let held = pool.alloc(448).unwrap();
            let error = log.write(None, &[bytes(2, len)]).await.unwrap_err();
            assert_eq!(error, Error::Pool(exhausted(3 * SECTOR, 192)));
            drop(held);
            log.write(None, &[bytes(2, len - 600)]).await.unwrap();
        })
        .unwrap();
        assert_eq!(stored(&mut sim, &node), Ok(records(&[340, len - 600])));
    }

    // A power cut in the write of a shorter record, which is in one sector: the bytes
    // of the stopped write are in the sector after it. Zeros with no sync of their own
    // can be lost when the record after them is not.
    #[test]
    fn a_power_cut_keeps_no_bytes_of_a_stopped_write_after_a_record() {
        let mut kept = [0, 0];
        for run in 0..128 {
            let (mut sim, node) = create_node(run);
            let phase = Arc::new(AtomicU64::new(0));
            let flag = Arc::clone(&phase);
            let own = node.clone();
            let handle = node.shards().start(shard("before"), move |_| async move {
                let (mut log, _, held) = stopped(&own).await;
                drop(held);
                flag.store(1, Ordering::Relaxed);
                log.write(None, &[bytes(2, 240)]).await.unwrap();
                flag.store(2, Ordering::Relaxed);
                pending::<()>().await;
            });
            drop(handle.unwrap());
            while phase.load(Ordering::Relaxed) == 0 {
                sim.run_for(Span::from_nanos(1_000)).unwrap();
            }
            sim.run_for(Span::from_nanos(2_000 * i64::try_from(run).unwrap()))
                .unwrap();
            let ended = phase.load(Ordering::Relaxed) == 2;
            sim.crash(&node, Crash::Power);

            let stored = stored(&mut sim, &node);
            let short = stored == Ok(records(&[1176, 240]));
            assert!(short || !ended, "run {run}");
            assert!(
                short || stored == Ok(records(&[1176])),
                "run {run}: {stored:?}"
            );
            kept[usize::from(short)] += 1;
        }
        assert!(kept[0] > 0 && kept[1] > 0, "{kept:?}");
    }

    // The record is at bytes 400 to 3,460, and its blocks end at bytes 1,536 and
    // 3,072. Bytes 512 to 3,072 are zeros before the write and twos after it.
    #[test]
    fn a_power_cut_keeps_each_sector_of_a_record_whole_or_not_at_all() {
        let mut split = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_node(run);
            sim.run_on(&node, |node, _| async move {
                let (mut log, _) = odd_open(&node).await.unwrap();
                log.write(None, &[bytes(1, 340)]).await.unwrap();
                node.fail_file(&file("log-0"), Operation::Sync);
                let error = log.write(None, &[bytes(2, 3000)]).await.unwrap_err();
                assert_eq!(error, io("log-0", Operation::Sync));
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            let whole = sim
                .run_on(&node, |node, _| async move {
                    let log = file("log-0");
                    let file = node.files().open(&log, Mode::Read).await.unwrap();
                    let bytes = read(&file, &create_pool()).await.unwrap();
                    bytes[SECTOR..6 * SECTOR]
                        .chunks(SECTOR)
                        .all(|sector| sector == [0; SECTOR] || sector == [2; SECTOR])
                })
                .unwrap();
            if !whole {
                split.push(run);
            }
        }
        assert_eq!(
            split,
            [],
            "the runs with a sector that holds a part of a write"
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
        let (mut sim, node) = create_node(0);
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
            (keys(), keys(), any::<u8>()).prop_map(
                |(incoming, outgoing, signature)| {
                    change(Voters { incoming, outgoing }, signature)
                }
            ),
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

    fn proofs() -> impl Strategy<Value = Option<Proof>> {
        let grant = prop::bool::ANY
            .prop_map(|vote| if vote { Grant::Vote } else { Grant::PreVote });
        let signature = prop::array::uniform(any::<u8>()).prop_map(Signature);
        let voters = prop::collection::btree_map(any::<u128>(), signature, 0..4);
        prop::option::of((grant, any::<u128>(), voters)).prop_map(|proof| {
            proof.map(|(grant, candidate, voters)| Proof {
                grant,
                candidate: key(candidate),
                voters: voters
                    .into_iter()
                    .map(|(voter, signature)| (key(voter), Some(signature)))
                    .collect(),
            })
        })
    }

    fn hards() -> impl Strategy<Value = Option<Hard>> {
        let keys = prop::option::of(any::<u128>());
        prop::option::of((any::<u64>(), keys.clone(), keys, proofs())).prop_map(
            |hard| {
                hard.map(|(term, vote, leader, proof)| Hard {
                    leader: leader.map(key),
                    proof,
                    ..self::hard(term, vote)
                })
            },
        )
    }

    proptest! {
        #[test]
        fn a_record_gives_back_what_it_was_made_from(
            number: u64,
            hard in hards(),
            entries in entries(),
        ) {
            let bytes = encode(number, hard.clone(), &entries);
            let Some(Record::Whole { number: read_number, body, after }) = record(&bytes) else {
                panic!("a record reads as a whole record");
            };
            prop_assert_eq!((read_number, after), (number, &[][..]));
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
            let whole = matches!(record(&bytes), Some(Record::Whole { number: 3, .. }));
            prop_assert!(!whole);
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
