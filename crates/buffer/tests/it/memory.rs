//! A file driver over memory, a stand-in until `sim` has files (#114).

use std::path::{Path, PathBuf};
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

    /// Puts `bytes` at `offset` of a file, as a crash or a defect would.
    ///
    /// # Panics
    ///
    /// When there is no file at `path`, or the bytes end past it.
    pub(crate) fn put(&self, path: &str, offset: usize, bytes: &[u8]) {
        let file = self.file(path);
        let mut file = lock(&file);
        file[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn file(&self, path: &str) -> Bytes {
        lock(&self.files)
            .get(Path::new(path))
            .cloned()
            .unwrap_or_else(|| panic!("no file at {path}"))
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
        let found = files.get(path).cloned();
        let result = match (found, mode) {
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
                files.insert(path.into(), Arc::clone(&bytes));
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
        lock(&self.files).remove(path);
        Box::pin(async { Ok(()) })
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
        let result = if files.contains_key(to) {
            Err(Error::Exists { path: to.into() })
        } else if let Some(bytes) = files.remove(from) {
            files.insert(to.into(), bytes);
            *lock(&self.path) = to.into();
            Ok(())
        } else {
            Err(Error::NotFound { path: from.into() })
        };
        Box::pin(async { result })
    }

    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async move { drop(self) })
    }
}
