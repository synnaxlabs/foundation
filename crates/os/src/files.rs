//! The `env::files` driver on the real disk. One I/O thread runs every call of a
//! handle, its clones, and their files, in the order of the calls.

use std::ffi::OsStr;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::thread;

use block::{Block, Unique};
use env::files::{Descriptor, Error, Mode, Operation, Request};
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

/// The flags that open a directory.
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// The `env::files` driver of one handle and its clones.
pub(crate) struct Driver {
    /// The data directory.
    data: Arc<OwnedFd>,
    thread: Thread,
}

impl Driver {
    /// Opens or makes `dir/data` and starts the I/O thread.
    pub(crate) fn new(dir: &Path) -> Result<Self, crate::Error> {
        let path = dir.join("data");
        std::fs::create_dir_all(&path).map_err(crate::Error::Dir)?;
        let open = |path| fs::open(path, DIRECTORY, fs::Mode::empty());
        // A `data` made here is not durable until `dir` syncs.
        let data = open(dir)
            .and_then(|dir| sync_all(&dir))
            .and_then(|()| open(&path))
            .map_err(|errno| crate::Error::Dir(errno.into()))?;
        Ok(Self {
            data: Arc::new(data),
            thread: Thread::start().map_err(crate::Error::Thread)?,
        })
    }

    /// Runs `call` on the data directory and `path` on the I/O thread.
    fn call<T: Send + 'static>(
        &self,
        path: &Path,
        call: impl FnOnce(&OwnedFd, &Path) -> Result<T, Error> + Send + 'static,
    ) -> Request<'_, T> {
        let (data, path) = (Arc::clone(&self.data), path.to_path_buf());
        Box::pin(self.thread.run(move || call(&data, &path)))
    }
}

impl env::files::Driver for Driver {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        let opened = self.call(path, move |data, path| open(data, path, mode));
        let (thread, path) = (self.thread.clone(), Arc::from(path));
        Box::pin(async move {
            let (fd, len) = opened.await?;
            let file: Box<dyn Descriptor> = Box::new(File {
                fd: Arc::new(fd),
                len,
                path,
                thread,
            });
            Ok(file)
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        self.call(dir, |data, dir| {
            list(data, dir).map_err(fail(dir, Operation::List))
        })
    }

    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.call(dir, |data, dir| {
            create_dir(data, dir).map_err(fail(dir, Operation::CreateDir))
        })
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        self.call(path, |data, path| {
            fs::unlinkat(data, path, AtFlags::empty())
                .map_err(fail(path, Operation::Remove))
        })
    }

    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.call(dir, |data, dir| {
            fs::openat(data, at(dir), DIRECTORY, fs::Mode::empty())
                .and_then(|fd| sync_all(&fd))
                .map_err(fail(dir, Operation::SyncDir))
        })
    }

    fn free(&self) -> Request<'_, u64> {
        self.call(Path::new(""), |data, path| {
            let stat = fs::fstatvfs(data).map_err(fail(path, Operation::Free))?;
            Ok(stat.f_bavail * stat.f_frsize)
        })
    }
}

/// One open file. Each call keeps the file open until it ends.
struct File {
    fd: Arc<OwnedFd>,
    len: u64,
    path: Arc<Path>,
    thread: Thread,
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
            self.thread
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
        let Self { fd, thread, .. } = *self;
        // The calls before this one have ended, so this drop closes the file.
        Box::pin(async move { thread.run(move || drop(fd)).await })
    }
}

/// A call that the I/O thread runs.
type Job = Box<dyn FnOnce() + Send>;

/// The I/O thread of one handle. It ends after its last sender drops.
#[derive(Clone)]
struct Thread(mpsc::Sender<Job>);

impl Thread {
    #[expect(clippy::disallowed_methods, reason = "os starts threads")]
    fn start() -> std::io::Result<Self> {
        let (jobs, mut received) = mpsc::channel::<Job>(DEPTH);
        thread::Builder::new().name("files".into()).spawn(move || {
            while let Some(job) = received.blocking_recv() {
                job();
            }
        })?;
        Ok(Self(jobs))
    }

    /// Runs `call` after every call sent before it. The future first waits for room
    /// in the queue; after that, `call` runs also when the future drops.
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
        Mode::Create { .. } => OFlags::RDWR | OFlags::CREATE,
    };
    let fd = fs::openat(data, path, flags | OFlags::CLOEXEC, FILE).map_err(&failed)?;
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
        // An empty file is one that a crash stopped before its allocation.
        Mode::Create { len } if found == 0 && len != 0 => {
            fs::fallocate(&fd, FallocateFlags::empty(), 0, len)
                .and_then(|()| sync_all(&fd))
                .map_err(&failed)?;
            Ok((fd, len))
        }
        _ => Ok((fd, found)),
    }
}

/// The names in `dir`, without `.` and `..`.
fn list(data: &OwnedFd, dir: &Path) -> io::Result<Vec<PathBuf>> {
    let fd = fs::openat(data, at(dir), DIRECTORY, fs::Mode::empty())?;
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

/// The error of `operation` on `path` for an OS error code.
fn fail(path: &Path, operation: Operation) -> impl Fn(Errno) -> Error + '_ {
    move |errno| match errno {
        Errno::NOENT => Error::NotFound {
            path: path.to_path_buf(),
        },
        Errno::NOSPC | Errno::DQUOT => Error::Full {
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
    use std::task::{Context, Waker};

    use super::*;

    /// The OS cannot sync a pipe.
    #[cfg(target_os = "linux")]
    const PIPE: Errno = Errno::INVAL;
    #[cfg(target_os = "macos")]
    const PIPE: Errno = Errno::BADF;

    #[test]
    fn a_sync_reaches_the_os() {
        let (pipe, _writer) = std::io::pipe().unwrap();
        let pipe = OwnedFd::from(pipe);
        assert_eq!(sync_data(&pipe), Err(PIPE));
        assert_eq!(sync_all(&pipe), Err(PIPE));
    }

    #[test]
    fn a_full_disk_gives_full_and_another_code_gives_io() {
        let fail = fail(Path::new("a"), Operation::WriteAt);
        let full = Error::Full { path: "a".into() };
        assert_eq!(fail(Errno::NOSPC), full);
        assert_eq!(fail(Errno::DQUOT), full);
        assert_eq!(fail(Errno::NOENT), Error::NotFound { path: "a".into() });
        let io = Error::Io {
            path: "a".into(),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(fail(Errno::IO), io);
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "the test holds the I/O thread")]
    fn a_call_past_the_depth_waits_outside_the_queue() {
        let thread = Thread::start().unwrap();
        let mut context = Context::from_waker(Waker::noop());
        let (started, running) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let mut first = Box::pin(thread.run(move || {
            started.send(()).unwrap();
            held.recv().unwrap();
        }));
        assert!(first.as_mut().poll(&mut context).is_pending());
        running.recv().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let mut calls: Vec<_> = (0..=DEPTH)
            .map(|_| {
                let ran = Arc::clone(&ran);
                Box::pin(thread.run(move || ran.fetch_add(1, Ordering::Relaxed)))
            })
            .collect();
        for call in &mut calls {
            assert!(call.as_mut().poll(&mut context).is_pending());
        }
        drop(calls);
        release.send(()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            first.await;
            thread.run(|| ()).await;
        });
        assert_eq!(ran.load(Ordering::Relaxed), DEPTH);
    }
}
