//! A file driver over memory. It counts syncs and can slow each one (#1016), it
//! can fail a write at an offset (#1104), and its calls end at once, which the
//! `alloc` binary needs (#2136). Only a test that needs this runs on it, until #517
//! and #2136 move it to `sim`.

use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use block::{Block, Unique};
use env::clock::Clock;
use env::files::{Descriptor, Driver, Error, Files, Mode, Operation, Request};
use types::hash;
use types::time::Span;

type Bytes = Arc<Mutex<Vec<u8>>>;

/// The files of one data directory, in memory. A clone shares them, so a reopen
/// reads what the last open wrote.
#[derive(Clone, Debug, Default)]
pub(crate) struct Memory {
    files: Arc<Mutex<hash::Map<PathBuf, Bytes>>>,
    syncs_fail: Arc<AtomicBool>,
    /// The offset of the next write to fail.
    failed_write: Arc<Mutex<Option<u64>>>,
    syncs: Arc<AtomicU64>,
    /// How many descriptors are open.
    opens: Arc<AtomicU64>,
    /// What every sync sleeps for, on a clock, before it ends.
    slow: Arc<Mutex<Option<(Clock, Span)>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Memory {
    pub(crate) fn files(&self) -> Files {
        Files::new(self.clone())
    }

    /// Every later sync fails with an I/O error.
    pub(crate) fn fail_syncs(&self) {
        self.syncs_fail.store(true, Relaxed);
    }

    /// The next write that starts at `offset` fails with an I/O error. Later
    /// writes pass.
    pub(crate) fn fail_write_at(&self, offset: u64) {
        *lock(&self.failed_write) = Some(offset);
    }

    /// Every later sync takes `span` of `clock`'s time, as a disk does.
    pub(crate) fn slow_syncs(&self, clock: Clock, span: Span) {
        *lock(&self.slow) = Some((clock, span));
    }

    /// How many syncs were asked for, failed ones included.
    pub(crate) fn syncs(&self) -> u64 {
        self.syncs.load(Relaxed)
    }

    /// How many descriptors are open now.
    pub(crate) fn open_files(&self) -> u64 {
        self.opens.load(Relaxed)
    }

    /// The bytes of a file.
    ///
    /// # Panics
    ///
    /// When there is no file at `path`.
    pub(crate) fn bytes(&self, path: &str) -> Vec<u8> {
        lock(&self.file(path)).clone()
    }

    fn file(&self, path: &str) -> Bytes {
        lock(&self.files)
            .get(&key(Path::new(path)))
            .cloned()
            .unwrap_or_else(|| panic!("no file at {path}"))
    }
}

/// The key of `path` in the map: its spelling with each `.` segment dropped, so
/// `./b` and `b` name one file, as on a disk.
fn key(path: &Path) -> PathBuf {
    path.components()
        .filter(|part| *part != Component::CurDir)
        .collect()
}

/// Whether `path` ends in `/` or `/.`, as `a/` does. Such a path names only a
/// directory. The driver gives `EISDIR` (21) on a create and `ENOTDIR` (20) on
/// a file that is there, as a disk does for a path ending in `/`.
fn slashed(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

/// Whether `path` is the data directory itself by a `.` segment, as `.` or `./`
/// is: a disk gives `EISDIR` on each file call. The empty path is not one: a disk
/// finds no file at it, a create too.
fn directory(path: &Path) -> bool {
    !path.as_os_str().is_empty() && key(path).as_os_str().is_empty()
}

fn io(path: &Path, operation: Operation, code: i32) -> Error {
    Error::Io {
        path: path.into(),
        operation,
        code,
    }
}

fn to_u64(len: usize) -> u64 {
    u64::try_from(len).expect("a length fits in u64")
}

fn to_usize(offset: u64) -> usize {
    usize::try_from(offset).expect("an offset fits in usize")
}

impl Driver for Memory {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        let mut files = lock(&self.files);
        let found = files.get(&key(path)).cloned();
        let result = match (found, mode) {
            _ if path.as_os_str().is_empty() => {
                Err(Error::NotFound { path: path.into() })
            }
            _ if directory(path) => Err(io(path, Operation::Open, 21)),
            (_, Mode::Create { .. }) if slashed(path) => {
                Err(io(path, Operation::Open, 21))
            }
            (Some(_), _) if slashed(path) => Err(io(path, Operation::Open, 20)),
            (Some(bytes), Mode::Create { len })
                if to_u64(lock(&bytes).len()) != len =>
            {
                Err(Error::Length {
                    path: path.into(),
                    expected: len,
                    found: to_u64(lock(&bytes).len()),
                })
            }
            (Some(bytes), _) => Ok(bytes),
            (None, Mode::Create { len }) => {
                let bytes = Arc::new(Mutex::new(vec![0; to_usize(len)]));
                files.insert(key(path), Arc::clone(&bytes));
                Ok(bytes)
            }
            (None, _) => Err(Error::NotFound { path: path.into() }),
        };
        let result = result.map(|bytes| {
            self.opens.fetch_add(1, Relaxed);
            let open: Box<dyn Descriptor> = Box::new(Open {
                bytes,
                files: Arc::clone(&self.files),
                path: Mutex::new(path.into()),
                syncs_fail: Arc::clone(&self.syncs_fail),
                failed_write: Arc::clone(&self.failed_write),
                syncs: Arc::clone(&self.syncs),
                opens: Arc::clone(&self.opens),
                slow: Arc::clone(&self.slow),
            });
            open
        });
        Box::pin(async { result })
    }

    fn list<'a>(&'a self, _: &'a Path) -> Request<'a, Vec<PathBuf>> {
        let mut names: Vec<PathBuf> = lock(&self.files).keys().cloned().collect();
        names.sort();
        Box::pin(async { Ok(names) })
    }

    fn create_dir<'a>(&'a self, _: &'a Path) -> Request<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        let mut files = lock(&self.files);
        let result = if directory(path) {
            Err(io(path, Operation::Remove, 21))
        } else if files.contains_key(&key(path)) && slashed(path) {
            Err(io(path, Operation::Remove, 20))
        } else {
            files.remove(&key(path));
            Ok(())
        };
        Box::pin(async { result })
    }

    fn sync_dir<'a>(&'a self, _: &'a Path) -> Request<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn free(&self) -> Request<'_, u64> {
        Box::pin(async { Ok(u64::MAX) })
    }
}

struct Open {
    bytes: Bytes,
    files: Arc<Mutex<hash::Map<PathBuf, Bytes>>>,
    /// The path of the file now: a rename changes it.
    path: Mutex<PathBuf>,
    syncs_fail: Arc<AtomicBool>,
    failed_write: Arc<Mutex<Option<u64>>>,
    syncs: Arc<AtomicU64>,
    opens: Arc<AtomicU64>,
    slow: Arc<Mutex<Option<(Clock, Span)>>>,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.opens.fetch_sub(1, Relaxed);
    }
}

impl Descriptor for Open {
    fn len(&self) -> u64 {
        to_u64(lock(&self.bytes).len())
    }

    fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()> {
        let failed = lock(&self.failed_write).take_if(|at| *at == offset);
        if failed.is_some() {
            let failed = io(&lock(&self.path), Operation::WriteAt, 5);
            return Box::pin(async { Err(failed) });
        }
        let mut bytes = lock(&self.bytes);
        let mut at = to_usize(offset);
        for part in parts {
            bytes[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        Box::pin(async { Ok(()) })
    }

    fn read_at(&self, offset: u64, mut into: Unique) -> Request<'_, Unique> {
        let bytes = lock(&self.bytes);
        let at = to_usize(offset);
        let len = into.len();
        into.copy_from_slice(&bytes[at..at + len]);
        Box::pin(async { Ok(into) })
    }

    fn sync(&self) -> Request<'_, ()> {
        self.syncs.fetch_add(1, Relaxed);
        let result = if self.syncs_fail.load(Relaxed) {
            Err(Error::Io {
                path: lock(&self.path).clone(),
                operation: Operation::Sync,
                code: 5,
            })
        } else {
            Ok(())
        };
        let slow = lock(&self.slow).clone();
        Box::pin(async move {
            if let Some((clock, span)) = slow {
                clock.sleep(span).await;
            }
            result
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> Request<'a, ()> {
        let mut files = lock(&self.files);
        let (old, new) = (key(from), key(to));
        let result = match files.get(&old) {
            Some(bytes) if !Arc::ptr_eq(bytes, &self.bytes) => {
                Err(Error::NotFound { path: from.into() })
            }
            None => Err(Error::NotFound { path: from.into() }),
            Some(_) if files.contains_key(&new) => {
                Err(Error::Exists { path: to.into() })
            }
            Some(_) => {
                let bytes = files.remove(&old).expect("invariant: `from` was found");
                files.insert(new, bytes);
                *lock(&self.path) = to.into();
                Ok(())
            }
        };
        Box::pin(async { result })
    }

    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async move { drop(self) })
    }

    fn remove(
        self: Box<Self>,
        _: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
        unreachable!("the buffer tests remove no file through a handle")
    }
}
