//! The `env::files` driver on the real disk. One I/O thread runs every call of a
//! disk and its files, in the order they reach its queue.

use std::ffi::OsStr;
use std::fmt;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use block::{Block, Unique};
use env::files::{Descriptor, Error, Mode, Operation, Request};
use env::thread::Handle;
use env::threads::Threads;
use rustix::fs::{self, AtFlags, FallocateFlags, FileType, FlockOperation, OFlags};
use rustix::io::{self, Errno};
use tokio::sync::{mpsc, oneshot};

/// The most parts that one `pwritev` takes: `IOV_MAX` on Linux and macOS.
const PARTS: usize = 1_024;

/// The calls that the queue of the I/O thread holds. A call past them waits for room,
/// so a stalled disk does not grow the queue.
const DEPTH: usize = 64;

/// The permissions of a new file.
const FILE: fs::Mode = fs::Mode::from_raw_mode(0o644);

/// The permissions of a new directory.
const DIR: fs::Mode = fs::Mode::from_raw_mode(0o755);

/// The flags that open a directory to read.
const READ_DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// The real disk under one data directory: the [`env::files::Driver`] that
/// [`files`](crate::files) gives. It is `Send`, so one thread can make it and another
/// wrap it with [`env::files::Files::new`].
pub struct Disk {
    /// The data directory.
    data: Arc<OwnedFd>,
    queue: Queue,
}

impl Disk {
    /// Opens or makes `dir/data` and starts I/O thread `name`.
    pub(crate) fn new(
        dir: &Path,
        threads: &Threads,
        name: &str,
    ) -> Result<(Self, Handle), crate::Error> {
        let data = data(dir).map_err(|errno| crate::Error::Dir(errno.into()))?;
        let (queue, thread) =
            Queue::start(threads, name).map_err(crate::Error::Thread)?;
        let disk = Self {
            data: Arc::new(data),
            queue,
        };
        Ok((disk, thread))
    }

    /// Runs `call` on the data directory and `path` on the I/O thread, as
    /// `operation`.
    fn call<T: Send + 'static>(
        &self,
        path: &Path,
        operation: Operation,
        call: impl FnOnce(&OwnedFd, &Path) -> io::Result<T> + Send + 'static,
    ) -> Request<'_, T> {
        let (data, path) = (Arc::clone(&self.data), path.to_path_buf());
        Box::pin(
            self.queue
                .run(move || call(&data, &path).map_err(fail(&path, operation))),
        )
    }
}

impl fmt::Debug for Disk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Disk").finish_non_exhaustive()
    }
}

impl env::files::Driver for Disk {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        let (data, owned) = (Arc::clone(&self.data), path.to_path_buf());
        let opened = self.queue.run(move || open(&data, &owned, mode));
        let (queue, path) = (self.queue.clone(), Arc::from(path));
        Box::pin(async move {
            let (fd, len) = opened.await?;
            let file: Box<dyn Descriptor> = Box::new(File {
                fd: Arc::new(fd),
                len,
                path,
                queue,
            });
            Ok(file)
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        self.call(dir, Operation::List, list)
    }

    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.call(dir, Operation::CreateDir, create_dir)
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        self.call(path, Operation::Remove, |data, path| {
            fs::unlinkat(data, path, AtFlags::empty())
        })
    }

    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.call(dir, Operation::SyncDir, |data, dir| {
            fs::openat(data, at(dir), READ_DIR, fs::Mode::empty())
                .and_then(|fd| sync_all(&fd))
        })
    }

    fn free(&self) -> Request<'_, u64> {
        self.call(Path::new(""), Operation::Free, |data, _| {
            fs::fstatvfs(data).map(|stat| stat.f_bavail * stat.f_frsize)
        })
    }
}

/// One open file. Each call keeps the file open until it ends.
struct File {
    fd: Arc<OwnedFd>,
    len: u64,
    path: Arc<Path>,
    queue: Queue,
}

impl File {
    /// Runs `call` on the file on the I/O thread, as `operation`.
    fn call<T: Send + 'static>(
        &self,
        operation: Operation,
        call: impl FnOnce(&OwnedFd) -> io::Result<T> + Send + 'static,
    ) -> Request<'_, T> {
        let (fd, path) = (Arc::clone(&self.fd), Arc::clone(&self.path));
        Box::pin(
            self.queue
                .run(move || call(&fd).map_err(fail(&path, operation))),
        )
    }
}

impl Descriptor for File {
    fn len(&self) -> u64 {
        self.len
    }

    fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()> {
        let parts = parts.to_vec();
        self.call(Operation::WriteAt, move |fd| write(fd, offset, &parts))
    }

    fn read_at(&self, offset: u64, into: Unique) -> Request<'_, Unique> {
        self.call(Operation::ReadAt, move |fd| {
            let mut into = into;
            read(fd, offset, &mut into)?;
            Ok(into)
        })
    }

    fn sync(&self) -> Request<'_, ()> {
        self.call(Operation::Sync, sync_data)
    }

    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
        let Self { fd, queue, .. } = *self;
        // The calls before this one have ended, so this drop closes the file.
        Box::pin(async move { queue.run(move || drop(fd)).await })
    }
}

/// A call that the I/O thread runs.
type Job = Box<dyn FnOnce() + Send>;

/// The queue of one I/O thread, which ends after the last clone drops.
#[derive(Clone)]
struct Queue(mpsc::Sender<Job>);

impl Queue {
    /// Starts I/O thread `name` and gives its queue and its handle.
    fn start(
        threads: &Threads,
        name: &str,
    ) -> Result<(Self, Handle), env::thread::Error> {
        let (jobs, mut queued) = mpsc::channel::<Job>(DEPTH);
        let thread = threads.start(name, move || async move {
            while let Some(job) = queued.recv().await {
                job();
            }
        })?;
        Ok((Self(jobs), thread))
    }

    /// Runs `call` after every call that reached the queue before it. The future
    /// first waits for room in the queue; after that, `call` runs also when the
    /// future drops.
    ///
    /// # Panics
    ///
    /// When a panic ended the I/O thread.
    async fn run<T: Send + 'static>(
        &self,
        call: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (reply, result) = oneshot::channel();
        // A dropped future does not want the result.
        let job: Job = Box::new(move || drop(reply.send(call())));
        let ended = "a panic ended the I/O thread of os::files";
        self.0.send(job).await.expect(ended);
        result.await.expect(ended)
    }
}

/// Opens the file at `path`, and gives it and its length. A write open locks the
/// file first, so a held file gives `Busy` before any other check.
fn open(data: &OwnedFd, path: &Path, mode: Mode) -> Result<(OwnedFd, u64), Error> {
    let failed = fail(path, Operation::Open);
    let flags = match mode {
        Mode::Read => OFlags::RDONLY,
        Mode::Write => OFlags::RDWR,
        Mode::Create { .. } => OFlags::RDWR.union(OFlags::CREATE),
    };
    let fd =
        fs::openat(data, path, flags.union(OFlags::CLOEXEC), FILE).map_err(&failed)?;
    if mode != Mode::Read {
        match fs::flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Err(Errno::WOULDBLOCK) => {
                return Err(Error::Busy {
                    path: path.to_path_buf(),
                });
            }
            locked => locked.map_err(&failed)?,
        }
    }
    let stat = fs::fstat(&fd).map_err(&failed)?;
    if FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(failed(Errno::ISDIR));
    }
    let found = stat.st_size.cast_unsigned();
    match mode {
        // Not atomic: a crash before the allocation leaves an empty file, which this
        // allocates.
        Mode::Create { len } if found == 0 && len != 0 => {
            fs::fallocate(&fd, FallocateFlags::empty(), 0, len)
                .and_then(|()| sync_all(&fd))
                .map_err(&failed)?;
            Ok((fd, len))
        }
        _ => Ok((fd, found)),
    }
}

/// Opens `dir/data`, and first makes it when it is not there.
fn data(dir: &Path) -> io::Result<OwnedFd> {
    let dir = fs::open(dir, READ_DIR, fs::Mode::empty())?;
    match fs::mkdirat(&dir, "data", DIR) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(errno) => return Err(errno),
    }
    // A `data` made by this or an earlier start is not durable until `dir` syncs.
    sync_all(&dir)?;
    fs::openat(&dir, "data", READ_DIR, fs::Mode::empty())
}

/// The names in `dir`, without `.` and `..`.
fn list(data: &OwnedFd, dir: &Path) -> io::Result<Vec<PathBuf>> {
    let fd = fs::openat(data, at(dir), READ_DIR, fs::Mode::empty())?;
    let mut names = Vec::new();
    for entry in fs::Dir::new(fd)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(PathBuf::from(OsStr::from_bytes(name)));
        }
    }
    Ok(names)
}

/// Makes the directory `dir`, or gives `Ok` when a directory is there.
fn create_dir(data: &OwnedFd, dir: &Path) -> io::Result<()> {
    match fs::mkdirat(data, at(dir), DIR) {
        Err(Errno::EXIST)
            if fs::statat(data, at(dir), AtFlags::empty())
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode).is_dir()) =>
        {
            Ok(())
        }
        made => made,
    }
}

/// Writes `parts` back to back at `offset`, in as many calls as the OS needs.
fn write(fd: &OwnedFd, mut offset: u64, parts: &[Block]) -> io::Result<()> {
    let mut slices: Vec<IoSlice<'_>> =
        parts.iter().map(|part| IoSlice::new(part)).collect();
    let mut rest = &mut slices[..];
    while !rest.is_empty() {
        let written = io::pwritev(fd, &rest[..rest.len().min(PARTS)], offset)?;
        offset += written as u64;
        IoSlice::advance_slices(&mut rest, written);
    }
    Ok(())
}

/// Fills `into` from `offset`. A file that something outside Foundation cut short
/// gives `EIO`.
fn read(fd: &OwnedFd, offset: u64, into: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < into.len() {
        match io::pread(fd, &mut into[done..], offset + done as u64)? {
            0 => return Err(Errno::IO),
            read => done += read,
        }
    }
    Ok(())
}

/// Makes the bytes of `fd` durable, through the cache of the disk.
fn sync_data(fd: &OwnedFd) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    return fs::fdatasync(fd);
    #[cfg(target_os = "macos")]
    return fs::fcntl_fullfsync(fd);
}

/// Makes the bytes and the metadata of `fd` durable, through the cache of the disk.
fn sync_all(fd: &OwnedFd) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    return fs::fsync(fd);
    #[cfg(target_os = "macos")]
    return fs::fcntl_fullfsync(fd);
}

/// The path of `path` from the data directory, where an empty path is the data
/// directory itself.
fn at(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

/// The error of `operation` on `path` for an OS error code. Only an operation that
/// takes space gives `Full`.
fn fail(path: &Path, operation: Operation) -> impl Fn(Errno) -> Error + '_ {
    let grows = matches!(
        operation,
        Operation::Open | Operation::CreateDir | Operation::WriteAt
    );
    move |errno| match errno {
        Errno::NOENT => Error::NotFound {
            path: path.to_path_buf(),
        },
        Errno::NOSPC | Errno::DQUOT if grows => Error::Full {
            path: path.to_path_buf(),
        },
        _ => Error::Io {
            path: path.to_path_buf(),
            operation,
            code: errno.raw_os_error(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Sender;
    use std::task::{Context, Waker};

    use env::files::Driver as _;

    use super::*;

    /// The OS cannot sync a pipe.
    #[cfg(target_os = "linux")]
    const PIPE: Errno = Errno::INVAL;
    #[cfg(target_os = "macos")]
    const PIPE: Errno = Errno::BADF;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    /// Queues a call that holds the I/O thread. Gives its future after the thread
    /// starts it, and the sender that ends it.
    #[expect(clippy::disallowed_methods, reason = "the test holds the I/O thread")]
    fn hold(queue: &Queue) -> (Pin<Box<impl Future<Output = ()>>>, Sender<()>) {
        let mut context = Context::from_waker(Waker::noop());
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let mut call = Box::pin(queue.run(move || {
            started.send(()).unwrap();
            released.recv().unwrap();
        }));
        assert!(call.as_mut().poll(&mut context).is_pending());
        running.recv().unwrap();
        (call, release)
    }

    #[test]
    fn a_sync_reaches_the_os() {
        let (pipe, _writer) = std::io::pipe().unwrap();
        let pipe = OwnedFd::from(pipe);
        assert_eq!(sync_data(&pipe), Err(PIPE));
        assert_eq!(sync_all(&pipe), Err(PIPE));
    }

    #[test]
    fn a_full_disk_gives_full_where_an_operation_takes_space() {
        let full = Error::Full { path: "a".into() };
        for operation in [Operation::Open, Operation::CreateDir, Operation::WriteAt] {
            let fail = fail(Path::new("a"), operation);
            assert_eq!(fail(Errno::NOSPC), full);
            assert_eq!(fail(Errno::DQUOT), full);
        }
        let fail = fail(Path::new("a"), Operation::Sync);
        let io = |code| Error::Io {
            path: "a".into(),
            operation: Operation::Sync,
            code,
        };
        assert_eq!(fail(Errno::NOSPC), io(Errno::NOSPC.raw_os_error()));
        assert_eq!(fail(Errno::DQUOT), io(Errno::DQUOT.raw_os_error()));
        assert_eq!(fail(Errno::NOENT), Error::NotFound { path: "a".into() });
        assert_eq!(fail(Errno::IO), io(5));
    }

    #[test]
    fn a_call_past_the_depth_waits_outside_the_queue() {
        let (queue, thread) =
            Queue::start(&crate::threads().unwrap(), "files").unwrap();
        let mut context = Context::from_waker(Waker::noop());
        let (held, release) = hold(&queue);
        let ran = Arc::new(AtomicUsize::new(0));
        let mut calls: Vec<_> = (0..=DEPTH)
            .map(|_| {
                let ran = Arc::clone(&ran);
                Box::pin(queue.run(move || ran.fetch_add(1, Ordering::Relaxed)))
            })
            .collect();
        for call in &mut calls {
            assert!(call.as_mut().poll(&mut context).is_pending());
        }
        drop(calls);
        release.send(()).unwrap();
        runtime().block_on(async {
            held.await;
            queue.run(|| ()).await;
        });
        assert_eq!(ran.load(Ordering::Relaxed), DEPTH);
        drop(queue);
        thread.join().unwrap();
    }

    #[test]
    fn close_ends_after_the_queued_calls_and_closes_the_file() {
        let dir = std::env::temp_dir()
            .join(format!("foundation-os-files-close-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let threads = crate::threads().unwrap();
        let (disk, thread) = Disk::new(&dir, &threads, "files").unwrap();
        let runtime = runtime();
        let file = runtime
            .block_on(disk.open(Path::new("a"), Mode::Create { len: 4_096 }))
            .unwrap();
        let mut context = Context::from_waker(Waker::noop());
        let (held, release) = hold(&disk.queue);
        let parts: [Block; 0] = [];
        let mut write = file.write_at(0, &parts);
        assert!(write.as_mut().poll(&mut context).is_pending());
        drop(write);
        let mut close = file.close();
        assert!(close.as_mut().poll(&mut context).is_pending());
        release.send(()).unwrap();
        runtime.block_on(async {
            held.await;
            close.await;
        });
        let other = fs::open(dir.join("data/a"), OFlags::RDWR, fs::Mode::empty());
        let lock = FlockOperation::NonBlockingLockExclusive;
        fs::flock(other.unwrap(), lock).unwrap();
        drop(disk);
        thread.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
