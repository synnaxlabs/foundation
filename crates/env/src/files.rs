//! Files under one data directory.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use block::{Block, Unique};

/// One driver call in flight, as the handle awaits it.
///
/// ```
/// let done: env::files::Request<'_, u64> = Box::pin(async { Ok(4_096) });
/// ```
pub type Request<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + 'a>>;

/// The length of a sector in bytes: the unit that the crash rule of [`File::write_at`]
/// keeps or loses whole. Each sector starts at a multiple of `SECTOR` in its file.
pub const SECTOR: usize = 512;

/// The files under one data directory. Paths are relative to it: a call panics on an
/// absolute path or a `..` segment. The handle cannot leave the thread that made it,
/// so each shard has its own. Clones use the same directory.
///
/// ```
/// use std::path::Path;
///
/// use env::files::{Error, Mode};
///
/// async fn ring(files: &env::files::Files) -> Result<env::files::File, Error> {
///     files.create_dir(Path::new("ring")).await?;
///     files.sync_dir(Path::new("")).await?;
///     let mode = Mode::Create { len: 1 << 26 };
///     let file = files.open(Path::new("ring/0"), mode).await?;
///     files.sync_dir(Path::new("ring")).await?;
///     Ok(file)
/// }
/// ```
#[derive(Clone)]
pub struct Files(Rc<dyn Driver>);

impl Files {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::files::Driver + 'static) -> env::files::Files {
    ///     env::files::Files::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Rc::new(driver))
    }

    /// Opens the file at `path`. A file that [`Mode::Create`] makes is not durable
    /// until [`Files::sync_dir`] on its directory ends. After that, a crash leaves it
    /// whole, with `len` zero bytes. A create that gives [`Error::Full`] leaves no file
    /// at `path` and keeps no blocks. Another error can leave an empty file at `path`,
    /// as a crash can.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] when the file is not there and `mode` is not
    ///   [`Mode::Create`].
    /// - [`Error::Busy`] when `mode` is not [`Mode::Read`] and another handle holds
    ///   the file with [`Mode::Write`] or [`Mode::Create`]. A handle holds it until it
    ///   drops and its calls end, in this process or another.
    /// - [`Error::Length`] when [`Mode::Create`] finds a file of another length that
    ///   is not empty. It treats an empty file that is there as missing and allocates
    ///   it, because a crash between the create and the allocation leaves one.
    /// - [`Error::Full`] when the disk has no room for the file that
    ///   [`Mode::Create`] allocates.
    /// - [`Error::Io`] for other failures.
    ///
    /// # Panics
    ///
    /// When `path` is absolute or has a `..` segment.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// async fn segment(files: &env::files::Files) -> Option<env::files::File> {
    ///     files.open(Path::new("segments/7"), env::files::Mode::Read).await.ok()
    /// }
    /// ```
    pub async fn open(&self, path: &Path, mode: Mode) -> Result<File, Error> {
        check(path);
        let descriptor = self.0.open(path, mode).await?;
        if let Mode::Create { len } = mode
            && descriptor.len() != len
        {
            return Err(Error::Length {
                path: path.to_path_buf(),
                expected: len,
                found: descriptor.len(),
            });
        }
        Ok(File {
            descriptor,
            path: path.to_path_buf(),
            mode,
            poisoned: Cell::new(false),
        })
    }

    /// The names of the files and directories directly in `dir`, sorted. Each is a
    /// bare name, not joined with `dir`. It does not list inside subdirectories.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `dir` is not there, and [`Error::Io`] for other
    /// failures.
    ///
    /// # Panics
    ///
    /// When `dir` is absolute or has a `..` segment.
    ///
    /// ```
    /// use std::path::{Path, PathBuf};
    ///
    /// async fn segments(files: &env::files::Files) -> Vec<PathBuf> {
    ///     files.list(Path::new("segments")).await.unwrap_or_default()
    /// }
    /// ```
    pub async fn list(&self, dir: &Path) -> Result<Vec<PathBuf>, Error> {
        check(dir);
        let mut names = self.0.list(dir).await?;
        names.sort_unstable();
        Ok(names)
    }

    /// Makes the directory `dir`. A directory that is there counts as made. It is not
    /// durable until [`Files::sync_dir`] on its parent ends.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] when the parent of `dir` is not there.
    /// - [`Error::Full`] when the disk has no room.
    /// - [`Error::Io`] for other failures.
    ///
    /// # Panics
    ///
    /// When `dir` is absolute or has a `..` segment.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// async fn make(files: &env::files::Files) -> Result<(), env::files::Error> {
    ///     files.create_dir(Path::new("segments")).await?;
    ///     files.sync_dir(Path::new("")).await
    /// }
    /// ```
    pub async fn create_dir(&self, dir: &Path) -> Result<(), Error> {
        check(dir);
        self.0.create_dir(dir).await
    }

    /// Removes the file at `path`. A file that is not there counts as removed. The
    /// removal is not durable until [`Files::sync_dir`] on its directory ends.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the OS cannot remove the file.
    ///
    /// # Panics
    ///
    /// When `path` is absolute or has a `..` segment.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// async fn evict(files: &env::files::Files) -> Result<(), env::files::Error> {
    ///     files.remove(Path::new("segments/7")).await?;
    ///     files.sync_dir(Path::new("segments")).await
    /// }
    /// ```
    pub async fn remove(&self, path: &Path) -> Result<(), Error> {
        check(path);
        match self.0.remove(path).await {
            Err(Error::NotFound { .. }) => Ok(()),
            result => result,
        }
    }

    /// Makes the files and directories created and removed in `dir` durable. An empty
    /// path is the data directory.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `dir` is not there, and [`Error::Io`] for other
    /// failures.
    ///
    /// # Panics
    ///
    /// When `dir` is absolute or has a `..` segment.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// async fn commit(files: &env::files::Files) -> Result<(), env::files::Error> {
    ///     files.sync_dir(Path::new("segments")).await
    /// }
    /// ```
    pub async fn sync_dir(&self, dir: &Path) -> Result<(), Error> {
        check(dir);
        self.0.sync_dir(dir).await
    }

    /// The bytes free on the disk of the data directory.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the OS cannot tell.
    ///
    /// ```
    /// async fn room(files: &env::files::Files) -> Result<u64, env::files::Error> {
    ///     files.free().await
    /// }
    /// ```
    pub async fn free(&self) -> Result<u64, Error> {
        self.0.free().await
    }
}

impl fmt::Debug for Files {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Files").finish_non_exhaustive()
    }
}

/// How [`Files::open`] opens a file.
///
/// ```
/// let mode = env::files::Mode::Create { len: 1 << 26 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Reads a file that is there.
    Read,
    /// Reads and writes a file that is there. One handle at a time writes a file; see
    /// [`Files::open`].
    Write,
    /// Reads and writes a file. When it is not there, makes it with `len` bytes,
    /// allocated and zeroed, and makes the allocation durable before the open ends. A
    /// file that is there keeps its bytes and must have `len` bytes. One handle at a
    /// time writes a file; see [`Files::open`].
    Create {
        /// The length of the file.
        len: u64,
    },
}

/// One open file. Its length does not change, and every read and write stays inside
/// it. A failed or dropped [`File::sync`] poisons the file: every later call fails
/// with [`Error::Poisoned`], because a second sync can report success for lost data.
/// Close the file, then reopen it and recover.
///
/// Calls may overlap in time. A [`File::sync`] covers the writes that ended before it
/// started. Where the ranges of calls in flight at the same time overlap, the bytes
/// are unknown.
///
/// Dropping it closes the file without a wait, after the calls of dropped futures
/// end. [`File::close`] waits for them.
///
/// ```
/// async fn commit(
///     file: &env::files::File,
///     offset: u64,
///     record: &[block::Block],
/// ) -> Result<(), env::files::Error> {
///     file.write_at(offset, record).await?;
///     file.sync().await
/// }
/// ```
pub struct File {
    descriptor: Box<dyn Descriptor>,
    path: PathBuf,
    mode: Mode,
    poisoned: Cell<bool>,
}

impl File {
    /// The length of the file in bytes.
    ///
    /// ```
    /// fn len(file: &env::files::File) -> u64 {
    ///     file.len()
    /// }
    /// ```
    #[must_use]
    #[expect(
        clippy::len_without_is_empty,
        reason = "an empty file has no use, so nothing asks"
    )]
    pub fn len(&self) -> u64 {
        self.descriptor.len()
    }

    /// Writes `parts` back to back at `offset`, as one vectored write. A read after
    /// the write ends sees the bytes. They are not durable until a later
    /// [`File::sync`] ends. A crash before then keeps any subset of the
    /// [sectors](SECTOR) of the write, independently of other writes not yet synced
    /// and of their order.
    ///
    /// The future may be dropped before it ends. The driver keeps a clone of each
    /// part until the write ends, so the drop is sound, but the bytes may then be
    /// written, partly written, or not written, the same as after a crash. Rewrite
    /// and sync that range before it is read as data.
    ///
    /// # Errors
    ///
    /// - [`Error::Poisoned`] after a failed or dropped sync.
    /// - [`Error::Full`] when the disk has no room for the bytes.
    /// - [`Error::Io`] for other failures.
    ///
    /// # Panics
    ///
    /// When the file was opened with [`Mode::Read`], or when the write ends past
    /// [`File::len`].
    ///
    /// ```
    /// async fn put(
    ///     file: &env::files::File,
    ///     parts: &[block::Block],
    /// ) -> Result<(), env::files::Error> {
    ///     file.write_at(4_096, parts).await
    /// }
    /// ```
    pub async fn write_at(&self, offset: u64, parts: &[Block]) -> Result<(), Error> {
        assert!(
            self.mode != Mode::Read,
            "write to {}, which was opened to read",
            self.path.display()
        );
        let len = parts.iter().map(|part| part.len()).sum();
        self.check_range("write", offset, len);
        self.check_poison()?;
        self.descriptor.write_at(offset, parts).await
    }

    /// Fills `into` with the bytes from `offset`, and gives it back.
    ///
    /// The future may be dropped before it ends. The driver keeps `into` until the
    /// read ends, so the drop is sound.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] after a failed or dropped sync, and [`Error::Io`] for other
    /// failures. `into` returns to its pool.
    ///
    /// # Panics
    ///
    /// When the read ends past [`File::len`].
    ///
    /// ```
    /// async fn get(
    ///     file: &env::files::File,
    ///     into: block::Unique,
    /// ) -> Result<block::Unique, env::files::Error> {
    ///     file.read_at(0, into).await
    /// }
    /// ```
    pub async fn read_at(&self, offset: u64, into: Unique) -> Result<Unique, Error> {
        self.check_range("read", offset, into.len());
        self.check_poison()?;
        self.descriptor.read_at(offset, into).await
    }

    /// Makes every write that ended before this call durable. It flushes the disk's
    /// cache too.
    ///
    /// # Errors
    ///
    /// [`Error::Poisoned`] after a failed or dropped sync, and [`Error::Io`] when the
    /// sync fails. Both poison the file, and so does a drop of the future before it
    /// ends.
    ///
    /// ```
    /// async fn commit(file: &env::files::File) -> Result<(), env::files::Error> {
    ///     file.sync().await
    /// }
    /// ```
    pub async fn sync(&self) -> Result<(), Error> {
        self.check_poison()?;
        let mut unfinished = Unfinished(Some(&self.poisoned));
        let result = self.descriptor.sync().await;
        unfinished.0 = None;
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }

    /// Makes the writes that ended before the call durable, as [`File::sync`], then
    /// gives the file the name `to` in the same directory. It never replaces a file.
    /// The errors of the handle then name `to`, and the handle keeps its hold: a write
    /// open of `to` gives [`Error::Busy`] until the handle closes.
    ///
    /// The new name is not durable until [`Files::sync_dir`] on the directory ends. A
    /// crash before then can undo the rename. A crash never leaves the file at both
    /// names, nor at neither when its old name was durable.
    ///
    /// A drop of the future before it ends poisons the handle, as for [`File::sync`].
    /// The rename can still end later, and then the file has the name `to`.
    ///
    /// # Errors
    ///
    /// - [`Error::Exists`] when `to` is there. Nothing changes.
    /// - [`Error::NotFound`] when the path of the handle no longer names its file:
    ///   another call removed or renamed the path. Nothing changes.
    /// - The errors of [`File::sync`]. The file keeps its name.
    /// - [`Error::Io`] for other failures, with the old path. A file system that cannot
    ///   rename with no replace gives it (Linux: `EINVAL`).
    ///
    /// # Panics
    ///
    /// When the file was opened with [`Mode::Read`], or when `to` is absolute, has a
    /// `..` segment, or is not a name in the directory of the file: in another
    /// directory, empty, `.`, or ending in `/` or `/.`.
    ///
    /// ```
    /// async fn publish(file: &mut env::files::File) -> Result<(), env::files::Error> {
    ///     file.rename(std::path::Path::new("ring")).await
    /// }
    /// ```
    pub async fn rename(&mut self, to: &Path) -> Result<(), Error> {
        assert!(
            self.mode != Mode::Read,
            "rename {}, which was opened to read",
            self.path.display()
        );
        check(to);
        let bytes = to.as_os_str().as_encoded_bytes();
        assert!(
            matches!(to.components().next_back(), Some(Component::Normal(_)))
                && !bytes.ends_with(b"/")
                && !bytes.ends_with(b"/.")
                && dir_of(to) == dir_of(&self.path),
            "rename {} to {}, which is not a name in the directory of the file",
            self.path.display(),
            to.display()
        );
        self.sync().await?;
        let mut unfinished = Unfinished(Some(&self.poisoned));
        let result = self.descriptor.rename(&self.path, to).await;
        unfinished.0 = None;
        if result.is_ok() {
            self.path = to.to_path_buf();
        }
        result
    }

    /// Closes the file. The future ends after every call of this handle ends, those
    /// of dropped futures too, and the file is closed. A write open of the same path
    /// then succeeds, unless another handle holds the file. A drop closes the handle
    /// too, but without a wait. After the drop of a write handle, a write open before
    /// its calls end gives [`Error::Busy`].
    ///
    /// It gives no error: [`File::sync`] makes the bytes durable, and a close after
    /// it loses nothing. A drop of the future closes the file without a wait.
    ///
    /// ```
    /// async fn reopen(
    ///     files: &env::files::Files,
    ///     file: env::files::File,
    ///     path: &std::path::Path,
    /// ) -> Result<env::files::File, env::files::Error> {
    ///     file.close().await;
    ///     files.open(path, env::files::Mode::Write).await
    /// }
    /// ```
    pub async fn close(self) {
        self.descriptor.close().await;
    }

    fn check_poison(&self) -> Result<(), Error> {
        if self.poisoned.get() {
            return Err(Error::Poisoned {
                path: self.path.clone(),
            });
        }
        Ok(())
    }

    fn check_range(&self, operation: &str, offset: u64, len: usize) {
        let file_len = self.len();
        let end = u64::try_from(len)
            .ok()
            .and_then(|len| offset.checked_add(len));
        assert!(
            end.is_some_and(|end| end <= file_len),
            "{operation} of {len} bytes at {offset} ends past the end of {} \
             ({file_len} bytes)",
            self.path.display()
        );
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File")
            .field("path", &self.path)
            .field("mode", &self.mode)
            .field("poisoned", &self.poisoned.get())
            .finish_non_exhaustive()
    }
}

/// Poisons the file when a sync future drops before it ends.
struct Unfinished<'a>(Option<&'a Cell<bool>>);

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        if let Some(poisoned) = self.0 {
            poisoned.set(true);
        }
    }
}

/// The names of the directory of `path`: its names but the last, `.` dropped.
fn dir_of(path: &Path) -> Vec<&OsStr> {
    let mut names: Vec<&OsStr> = (path.components())
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    names.pop();
    names
}

/// Panics on a path that leaves the data directory.
fn check(path: &Path) {
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => panic!(
                "path {} is absolute; paths are relative to the data directory",
                path.display()
            ),
            Component::ParentDir => panic!(
                "path {} has a `..` segment; paths stay inside the data directory",
                path.display()
            ),
            Component::CurDir | Component::Normal(_) => {}
        }
    }
}

/// Why a file call failed. Paths are relative to the data directory.
///
/// ```
/// use std::path::PathBuf;
///
/// let e = env::files::Error::NotFound { path: PathBuf::from("ring/0") };
/// assert_eq!(e.to_string(), "path ring/0 is not there");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file or directory is not there.
    NotFound {
        /// The path of the call.
        path: PathBuf,
    },
    /// The disk has no room.
    Full {
        /// The path of the call.
        path: PathBuf,
    },
    /// A sync of this file failed or was dropped earlier. Close it, then reopen it and
    /// recover.
    Poisoned {
        /// The path of the file.
        path: PathBuf,
    },
    /// Another handle holds the file with [`Mode::Write`] or [`Mode::Create`].
    Busy {
        /// The path of the file.
        path: PathBuf,
    },
    /// [`File::rename`] found a file or directory at its new name.
    Exists {
        /// The new name.
        path: PathBuf,
    },
    /// [`Mode::Create`] found a file of another length.
    Length {
        /// The path of the file.
        path: PathBuf,
        /// The length that the call asked for.
        expected: u64,
        /// The length of the file.
        found: u64,
    },
    /// The OS or the simulation reported another failure.
    Io {
        /// The path of the call. It is empty for [`Files::free`].
        path: PathBuf,
        /// The call that failed.
        operation: Operation,
        /// The OS error code.
        code: i32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => {
                write!(f, "path {} is not there", path.display())
            }
            Self::Full { path } => {
                write!(f, "no room on the disk for {}", path.display())
            }
            Self::Poisoned { path } => write!(
                f,
                "a sync of file {} failed or was dropped; close it, then reopen it and \
                 recover",
                path.display()
            ),
            Self::Busy { path } => write!(
                f,
                "file {} is open for writing in another handle",
                path.display()
            ),
            Self::Exists { path } => {
                write!(f, "path {} is already there", path.display())
            }
            Self::Length {
                path,
                expected,
                found,
            } => write!(
                f,
                "file {} has {found} bytes, but {expected} bytes were expected",
                path.display()
            ),
            Self::Io {
                path,
                operation,
                code,
            } => write!(
                f,
                "{operation} of {} failed with OS error {code}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The call that an [`Error::Io`] comes from.
///
/// ```
/// assert_eq!(env::files::Operation::SyncDir.to_string(), "sync_dir");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// [`Files::open`].
    Open,
    /// [`Files::list`].
    List,
    /// [`Files::create_dir`].
    CreateDir,
    /// [`Files::remove`].
    Remove,
    /// [`Files::sync_dir`].
    SyncDir,
    /// [`Files::free`].
    Free,
    /// [`File::write_at`].
    WriteAt,
    /// [`File::read_at`].
    ReadAt,
    /// [`File::sync`].
    Sync,
    /// [`File::rename`].
    Rename,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::List => "list",
            Self::CreateDir => "create_dir",
            Self::Remove => "remove",
            Self::SyncDir => "sync_dir",
            Self::Free => "free",
            Self::WriteAt => "write_at",
            Self::ReadAt => "read_at",
            Self::Sync => "sync",
            Self::Rename => "rename",
        })
    }
}

/// What `os` and `sim` implement to run [`Files`]. Only they implement it.
///
/// Paths reach it checked and relative to the data directory. It opens, creates, and
/// removes files with the rules of the [`Files`] calls; [`Files`] sorts and checks
/// what it returns.
///
/// ```
/// fn wrap(driver: impl env::files::Driver + 'static) -> env::files::Files {
///     env::files::Files::new(driver)
/// }
/// ```
pub trait Driver {
    /// Opens the file at `path`. [`Mode::Create`] makes a missing file with `len`
    /// zeroed bytes. It treats an empty file that is there as missing and allocates
    /// it, because a crash between the create and the allocation leaves one. It opens
    /// any other file that is there as it is. A create that gives [`Error::Full`]
    /// leaves no file at `path` and keeps no blocks. Another error can leave an empty
    /// file at `path`, as a crash can. It makes the allocation durable before it ends
    /// (`os`: `fallocate`, then `fsync` the file), so a `sync_dir` alone makes the file
    /// whole. A write open of a file that a write handle holds gives [`Error::Busy`]
    /// before any other check or change of the file.
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>>;

    /// The bare names of the files and directories directly in `dir`, in any order.
    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>>;

    /// Makes the directory `dir`, or gives `Ok` when a directory is there.
    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()>;

    /// Removes the file at `path`, or gives [`Error::NotFound`].
    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()>;

    /// Makes the creates and removes in `dir`, of files and directories, durable.
    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()>;

    /// The bytes free on the disk of the data directory.
    fn free(&self) -> Request<'_, u64>;
}

/// One open file as a driver holds it, run through a [`File`]. Only `os` and `sim`
/// implement it.
///
/// A [`Request`] may be dropped before it ends. The descriptor then keeps the blocks
/// of the call (a clone of each [`Block`], or the [`Unique`]) until the call ends.
/// Dropping the descriptor closes the file without a wait, after its calls end.
///
/// Its calls follow the overlap and crash rules of [`File`] and [`File::write_at`];
/// `sim` models exactly those rules.
///
/// ```
/// fn len(descriptor: &dyn env::files::Descriptor) -> u64 {
///     descriptor.len()
/// }
/// ```
#[expect(
    clippy::len_without_is_empty,
    reason = "an empty file has no use, so nothing asks"
)]
pub trait Descriptor {
    /// The length of the file in bytes. It does not change.
    fn len(&self) -> u64;

    /// Writes `parts` back to back at `offset`, which [`File`] has checked.
    fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()>;

    /// Fills `into` from `offset`, which [`File`] has checked.
    fn read_at(&self, offset: u64, into: Unique) -> Request<'_, Unique>;

    /// Makes the writes that ended before the call durable.
    fn sync(&self) -> Request<'_, ()>;

    /// Renames `from` to `to` in one directory when `from` names this file, with no
    /// replace, and keeps the holds of the file. It gives [`Error::NotFound`] when
    /// `from` names another file or none, and [`Error::Exists`] when `to` is there,
    /// and then changes nothing. After `Ok`, the errors of later calls name `to`.
    /// [`File`] has checked the paths and made the writes durable.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> Request<'a, ()>;

    /// Closes the file. The future ends after the calls of the descriptor end and the
    /// file is closed.
    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>>;
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// Answers every call at once. Files have `len` bytes, and syncs give `sync`.
    struct Fixed {
        len: u64,
        sync: Result<(), Error>,
        names: Vec<PathBuf>,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl Fixed {
        fn files(len: u64) -> (Files, Rc<RefCell<Vec<String>>>) {
            Self::with_sync(len, Ok(()))
        }

        fn with_sync(
            len: u64,
            sync: Result<(), Error>,
        ) -> (Files, Rc<RefCell<Vec<String>>>) {
            let calls = Rc::default();
            let driver = Self {
                len,
                sync,
                names: vec!["b".into(), "c".into(), "a".into()],
                calls: Rc::clone(&calls),
            };
            (Files::new(driver), calls)
        }

        fn record(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl Driver for Fixed {
        fn open<'a>(
            &'a self,
            path: &'a Path,
            mode: Mode,
        ) -> Request<'a, Box<dyn Descriptor>> {
            self.record(format!("open {} {mode:?}", path.display()));
            let descriptor: Box<dyn Descriptor> = Box::new(Fixed {
                len: self.len,
                sync: self.sync.clone(),
                names: Vec::new(),
                calls: Rc::clone(&self.calls),
            });
            Box::pin(async { Ok(descriptor) })
        }

        fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
            self.record(format!("list {}", dir.display()));
            Box::pin(async { Ok(self.names.clone()) })
        }

        fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
            self.record(format!("create_dir {}", dir.display()));
            Box::pin(async { Ok(()) })
        }

        fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
            self.record(format!("remove {}", path.display()));
            let result = match path.to_str() {
                Some("gone") => Err(Error::NotFound { path: path.into() }),
                Some("locked") => Err(Error::Io {
                    path: path.into(),
                    operation: Operation::Remove,
                    code: 13,
                }),
                _ => Ok(()),
            };
            Box::pin(async { result })
        }

        fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
            self.record(format!("sync_dir {}", dir.display()));
            Box::pin(async { Ok(()) })
        }

        fn free(&self) -> Request<'_, u64> {
            Box::pin(async { Ok(7) })
        }
    }

    impl Descriptor for Fixed {
        fn len(&self) -> u64 {
            self.len
        }

        fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()> {
            self.record(format!("write_at {offset} {}", parts.len()));
            Box::pin(async { Ok(()) })
        }

        fn read_at(&self, _: u64, into: Unique) -> Request<'_, Unique> {
            Box::pin(async { Ok(into) })
        }

        fn sync(&self) -> Request<'_, ()> {
            self.record("sync".into());
            Box::pin(async { self.sync.clone() })
        }

        fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> Request<'a, ()> {
            self.record(format!("rename {} {}", from.display(), to.display()));
            let result = match to.file_name().and_then(OsStr::to_str) {
                Some("taken") => Err(Error::Exists { path: to.into() }),
                Some("gone") => Err(Error::NotFound { path: from.into() }),
                _ => Ok(()),
            };
            Box::pin(async { result })
        }

        fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
            self.record("close".into());
            Box::pin(async {})
        }
    }

    /// Never ends a call of one operation.
    /// A driver that hangs the call of `on` after `passed` calls of it ended.
    struct Stuck {
        on: Operation,
        passed: Cell<u32>,
    }

    impl Stuck {
        fn file(on: Operation) -> File {
            Self::file_after(on, 0)
        }

        fn file_after(on: Operation, passed: u32) -> File {
            File {
                descriptor: Box::new(Self {
                    on,
                    passed: Cell::new(passed),
                }),
                path: "ring/0".into(),
                mode: Mode::Write,
                poisoned: Cell::new(false),
            }
        }

        fn request(&self, operation: Operation) -> Request<'_, ()> {
            if self.on == operation {
                if self.passed.get() == 0 {
                    return Box::pin(std::future::pending());
                }
                self.passed.set(self.passed.get() - 1);
            }
            Box::pin(async { Ok(()) })
        }
    }

    impl Descriptor for Stuck {
        fn len(&self) -> u64 {
            0
        }

        fn write_at<'a>(&'a self, _: u64, _: &'a [Block]) -> Request<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn read_at(&self, _: u64, into: Unique) -> Request<'_, Unique> {
            Box::pin(async { Ok(into) })
        }

        fn sync(&self) -> Request<'_, ()> {
            self.request(Operation::Sync)
        }

        fn rename<'a>(&'a self, _: &'a Path, _: &'a Path) -> Request<'a, ()> {
            self.request(Operation::Rename)
        }

        fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
            Box::pin(async {})
        }
    }

    /// Polls `future` once, which must leave it pending, and drops it.
    fn drop_pending(future: impl Future) {
        let mut future = Box::pin(future);
        let poll = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(poll.is_pending(), "the future ended");
    }

    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the future is pending"),
        }
    }

    fn open(files: &Files, mode: Mode) -> File {
        ready(files.open(Path::new("ring/0"), mode)).expect("the driver opens")
    }

    fn io(operation: Operation) -> Error {
        Error::Io {
            path: "ring/0".into(),
            operation,
            code: 5,
        }
    }

    mod paths {
        use super::*;

        #[test]
        #[should_panic(
            expected = "path /ring/0 is absolute; paths are relative to the data \
                        directory"
        )]
        fn open_panics_on_an_absolute_path() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.open(Path::new("/ring/0"), Mode::Read)));
        }

        #[test]
        #[should_panic(
            expected = "path ring/../../etc has a `..` segment; paths stay inside the \
                        data directory"
        )]
        fn open_panics_on_a_parent_segment() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.open(Path::new("ring/../../etc"), Mode::Read)));
        }

        #[test]
        #[should_panic(expected = "path /segments is absolute")]
        fn list_panics_on_an_absolute_path() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.list(Path::new("/segments"))));
        }

        #[test]
        #[should_panic(expected = "path ../7 has a `..` segment")]
        fn remove_panics_on_a_parent_segment() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.remove(Path::new("../7"))));
        }

        #[test]
        #[should_panic(expected = "path segments/.. has a `..` segment")]
        fn create_dir_panics_on_a_parent_segment() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.create_dir(Path::new("segments/.."))));
        }

        #[test]
        #[should_panic(expected = "path /segments is absolute")]
        fn sync_dir_panics_on_an_absolute_path() {
            let (files, _) = Fixed::files(0);
            drop(ready(files.sync_dir(Path::new("/segments"))));
        }

        #[test]
        fn passes_relative_paths_to_the_driver() {
            let (files, calls) = Fixed::files(8);
            open(&files, Mode::Write);
            ready(files.list(Path::new(""))).expect("the driver lists");
            ready(files.create_dir(Path::new("segments"))).expect("the driver makes");
            ready(files.remove(Path::new("./segments/7"))).expect("the driver removes");
            ready(files.sync_dir(Path::new("segments"))).expect("the driver syncs");
            assert_eq!(
                *calls.borrow(),
                [
                    "open ring/0 Write",
                    "list ",
                    "create_dir segments",
                    "remove ./segments/7",
                    "sync_dir segments"
                ]
            );
        }
    }

    mod open {
        use super::*;

        #[test]
        fn creates_a_file_of_the_asked_length() {
            let (files, _) = Fixed::files(4_096);
            assert_eq!(open(&files, Mode::Create { len: 4_096 }).len(), 4_096);
        }

        #[test]
        fn fails_when_create_finds_another_length() {
            let (files, _) = Fixed::files(512);
            let e = ready(files.open(Path::new("ring/0"), Mode::Create { len: 4_096 }))
                .expect_err("the lengths differ");
            assert_eq!(
                e,
                Error::Length {
                    path: "ring/0".into(),
                    expected: 4_096,
                    found: 512,
                }
            );
        }

        #[test]
        fn opens_a_file_of_any_length_to_write() {
            let (files, _) = Fixed::files(512);
            assert_eq!(open(&files, Mode::Write).len(), 512);
        }
    }

    mod remove {
        use super::*;

        #[test]
        fn succeeds_when_the_file_is_not_there() {
            let (files, calls) = Fixed::files(0);
            assert_eq!(ready(files.remove(Path::new("gone"))), Ok(()));
            assert_eq!(*calls.borrow(), ["remove gone"]);
        }

        #[test]
        fn gives_other_errors() {
            let (files, _) = Fixed::files(0);
            assert_eq!(
                ready(files.remove(Path::new("locked"))),
                Err(Error::Io {
                    path: "locked".into(),
                    operation: Operation::Remove,
                    code: 13,
                })
            );
        }
    }

    mod list {
        use super::*;

        #[test]
        fn sorts_the_names() {
            let (files, _) = Fixed::files(0);
            let names =
                ready(files.list(Path::new("segments"))).expect("the driver lists");
            assert_eq!(names, [Path::new("a"), Path::new("b"), Path::new("c")]);
        }
    }

    mod free {
        use super::*;

        #[test]
        fn gives_the_bytes_that_the_driver_finds() {
            let (files, _) = Fixed::files(0);
            assert_eq!(ready(files.free()), Ok(7));
        }
    }

    mod debug {
        use super::*;

        #[test]
        fn shows_the_path_mode_and_poison_of_a_file() {
            let (files, _) = Fixed::files(8);
            assert_eq!(format!("{files:?}"), "Files { .. }");
            let file = open(&files, Mode::Write);
            assert_eq!(
                format!("{file:?}"),
                r#"File { path: "ring/0", mode: Write, poisoned: false, .. }"#
            );
        }
    }

    mod write_at {
        use super::*;

        #[test]
        fn forwards_a_write_inside_the_file() {
            let (files, calls) = Fixed::files(8);
            let file = open(&files, Mode::Write);
            ready(file.write_at(8, &[])).expect("the driver writes");
            assert_eq!(
                calls.borrow().last().map(String::as_str),
                Some("write_at 8 0")
            );
        }

        #[test]
        #[should_panic(
            expected = "write of 0 bytes at 9 ends past the end of ring/0 (8 bytes)"
        )]
        fn panics_past_the_end() {
            let (files, _) = Fixed::files(8);
            let file = open(&files, Mode::Write);
            drop(ready(file.write_at(9, &[])));
        }

        #[test]
        #[should_panic(expected = "write to ring/0, which was opened to read")]
        fn panics_on_a_file_opened_to_read() {
            let (files, _) = Fixed::files(8);
            let file = open(&files, Mode::Read);
            drop(ready(file.write_at(0, &[])));
        }
    }

    mod sync {
        use super::*;

        #[test]
        fn leaves_the_file_usable_after_a_success() {
            let (files, calls) = Fixed::files(8);
            let file = open(&files, Mode::Write);
            ready(file.sync()).expect("the sync succeeds");
            ready(file.sync()).expect("the sync succeeds");
            assert_eq!(calls.borrow()[1..], ["sync", "sync"]);
        }

        #[test]
        fn poisons_the_file_when_it_fails() {
            let (files, calls) = Fixed::with_sync(8, Err(io(Operation::Sync)));
            let file = open(&files, Mode::Write);
            assert_eq!(ready(file.sync()), Err(io(Operation::Sync)));
            let poisoned = Err(Error::Poisoned {
                path: "ring/0".into(),
            });
            assert_eq!(ready(file.sync()), poisoned);
            assert_eq!(ready(file.write_at(0, &[])), poisoned);
            assert_eq!(
                calls.borrow()[1..],
                ["sync"],
                "a poisoned call reached the driver"
            );
        }

        #[test]
        fn poisons_the_file_when_dropped_before_it_ends() {
            let file = Stuck::file(Operation::Sync);
            drop_pending(file.sync());
            assert_eq!(
                ready(file.write_at(0, &[])),
                Err(Error::Poisoned {
                    path: "ring/0".into()
                })
            );
        }
    }

    mod rename {
        use super::*;

        fn poisoned(path: &str) -> Result<(), Error> {
            Err(Error::Poisoned { path: path.into() })
        }

        #[test]
        fn syncs_then_renames_and_the_handle_takes_the_new_path() {
            let (files, calls) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            ready(file.rename(Path::new("ring/1"))).expect("the driver renames");
            assert_eq!(calls.borrow()[1..], ["sync", "rename ring/0 ring/1"]);
            ready(file.rename(Path::new("ring/2"))).expect("the driver renames");
            assert_eq!(calls.borrow()[3..], ["sync", "rename ring/1 ring/2"]);
        }

        #[test]
        fn accepts_a_current_directory_segment() {
            let (files, calls) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            ready(file.rename(Path::new("./ring/1"))).expect("the driver renames");
            assert_eq!(calls.borrow()[2], "rename ring/0 ./ring/1");
        }

        #[test]
        fn an_error_after_the_rename_names_the_new_path() {
            let mut file = Stuck::file_after(Operation::Sync, 1);
            ready(file.rename(Path::new("ring/1"))).expect("the driver renames");
            drop_pending(file.sync());
            assert_eq!(ready(file.write_at(0, &[])), poisoned("ring/1"));
        }

        #[test]
        fn a_poisoned_file_does_not_rename() {
            let mut file = Stuck::file(Operation::Sync);
            drop_pending(file.sync());
            assert_eq!(ready(file.rename(Path::new("ring/1"))), poisoned("ring/0"));
        }

        #[test]
        fn a_failed_sync_keeps_the_name_and_poisons() {
            let (files, calls) = Fixed::with_sync(8, Err(io(Operation::Sync)));
            let mut file = open(&files, Mode::Write);
            assert_eq!(
                ready(file.rename(Path::new("ring/1"))),
                Err(io(Operation::Sync))
            );
            assert_eq!(
                calls.borrow()[1..],
                ["sync"],
                "a failed sync reached rename"
            );
            assert_eq!(ready(file.rename(Path::new("ring/1"))), poisoned("ring/0"));
        }

        #[test]
        fn exists_and_not_found_keep_the_name_and_do_not_poison() {
            let (files, calls) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            assert_eq!(
                ready(file.rename(Path::new("ring/taken"))),
                Err(Error::Exists {
                    path: "ring/taken".into()
                })
            );
            assert_eq!(
                ready(file.rename(Path::new("ring/gone"))),
                Err(Error::NotFound {
                    path: "ring/0".into()
                })
            );
            ready(file.rename(Path::new("ring/1"))).expect("the driver renames");
            assert_eq!(
                calls.borrow()[1..],
                [
                    "sync",
                    "rename ring/0 ring/taken",
                    "sync",
                    "rename ring/0 ring/gone",
                    "sync",
                    "rename ring/0 ring/1"
                ]
            );
        }

        #[test]
        fn poisons_the_file_when_dropped_before_it_ends() {
            let mut file = Stuck::file(Operation::Rename);
            drop_pending(file.rename(Path::new("ring/1")));
            assert_eq!(ready(file.write_at(0, &[])), poisoned("ring/0"));
        }

        #[test]
        #[should_panic(expected = "rename ring/0, which was opened to read")]
        fn panics_on_a_file_opened_to_read() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Read);
            drop(ready(file.rename(Path::new("ring/1"))));
        }

        #[test]
        #[should_panic(expected = "rename ring/0 to segments/0, which is not a name")]
        fn panics_on_a_path_in_another_directory() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("segments/0"))));
        }

        #[test]
        #[should_panic(expected = "rename a to , which is not a name")]
        fn panics_on_an_empty_path() {
            let (files, _) = Fixed::files(8);
            let mut file = ready(files.open(Path::new("a"), Mode::Write)).unwrap();
            drop(ready(file.rename(Path::new(""))));
        }

        #[test]
        #[should_panic(expected = "rename ring/0 to ring/1/, which is not a name")]
        fn panics_on_a_trailing_slash() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("ring/1/"))));
        }

        #[test]
        #[should_panic(expected = "rename ring/0 to ring/1/., which is not a name")]
        fn panics_on_a_trailing_dot() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("ring/1/."))));
        }

        #[test]
        #[should_panic(expected = "rename a to ., which is not a name")]
        fn panics_on_the_current_directory() {
            let (files, _) = Fixed::files(8);
            let mut file = ready(files.open(Path::new("a"), Mode::Write)).unwrap();
            drop(ready(file.rename(Path::new("."))));
        }

        #[test]
        #[should_panic(expected = "rename ring/0 to ./, which is not a name")]
        fn panics_on_the_current_directory_with_a_slash() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("./"))));
        }

        #[test]
        #[should_panic(expected = "rename ring/0 to ring, which is not a name")]
        fn panics_on_the_directory_of_the_file() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("ring"))));
        }

        #[test]
        #[should_panic(expected = "path /ring/1 is absolute")]
        fn panics_on_an_absolute_path() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("/ring/1"))));
        }

        #[test]
        #[should_panic(expected = "path ring/../ring/1 has a `..` segment")]
        fn panics_on_a_parent_segment() {
            let (files, _) = Fixed::files(8);
            let mut file = open(&files, Mode::Write);
            drop(ready(file.rename(Path::new("ring/../ring/1"))));
        }
    }

    mod close {
        use super::*;

        #[test]
        fn closes_the_descriptor_once() {
            let (files, calls) = Fixed::files(8);
            ready(open(&files, Mode::Write).close());
            assert_eq!(calls.borrow()[1..], ["close"]);
        }

        #[test]
        fn closes_a_poisoned_file() {
            let (files, calls) = Fixed::with_sync(8, Err(io(Operation::Sync)));
            let file = open(&files, Mode::Write);
            assert_eq!(ready(file.sync()), Err(io(Operation::Sync)));
            ready(file.close());
            assert_eq!(calls.borrow()[1..], ["sync", "close"]);
        }
    }

    mod check_range {
        use super::*;

        fn file(len: u64) -> File {
            File {
                descriptor: Box::new(Fixed {
                    len,
                    sync: Ok(()),
                    names: Vec::new(),
                    calls: Rc::default(),
                }),
                path: "ring/0".into(),
                mode: Mode::Write,
                poisoned: Cell::new(false),
            }
        }

        #[test]
        fn accepts_a_range_that_ends_at_the_end() {
            file(4_096).check_range("read", 4_000, 96);
        }

        #[test]
        #[should_panic(
            expected = "read of 97 bytes at 4000 ends past the end of ring/0 \
                        (4096 bytes)"
        )]
        fn panics_on_a_range_one_byte_too_long() {
            file(4_096).check_range("read", 4_000, 97);
        }

        #[test]
        #[should_panic(
            expected = "read of 2 bytes at 18446744073709551615 ends past the end"
        )]
        fn panics_when_the_end_overflows() {
            file(u64::MAX).check_range("read", u64::MAX, 2);
        }
    }

    mod error {
        use super::*;

        #[test]
        fn names_the_path_when_the_disk_is_full() {
            let e = Error::Full {
                path: "segments/7".into(),
            };
            assert_eq!(e.to_string(), "no room on the disk for segments/7");
        }

        #[test]
        fn tells_how_to_recover_from_poison() {
            let e = Error::Poisoned {
                path: "ring/0".into(),
            };
            assert_eq!(
                e.to_string(),
                "a sync of file ring/0 failed or was dropped; close it, then reopen \
                 it and recover"
            );
        }

        #[test]
        fn names_both_lengths() {
            let e = Error::Length {
                path: "ring/0".into(),
                expected: 4_096,
                found: 512,
            };
            assert_eq!(
                e.to_string(),
                "file ring/0 has 512 bytes, but 4096 bytes were expected"
            );
        }

        #[test]
        fn says_another_handle_writes_the_file() {
            let e = Error::Busy {
                path: "ring/0".into(),
            };
            assert_eq!(
                e.to_string(),
                "file ring/0 is open for writing in another handle"
            );
        }

        #[test]
        fn names_create_dir() {
            assert_eq!(Operation::CreateDir.to_string(), "create_dir");
        }

        #[test]
        fn says_the_new_name_is_taken() {
            let e = Error::Exists {
                path: "ring".into(),
            };
            assert_eq!(e.to_string(), "path ring is already there");
            assert_eq!(Operation::Rename.to_string(), "rename");
        }

        #[test]
        fn names_the_operation_and_the_code() {
            assert_eq!(
                io(Operation::WriteAt).to_string(),
                "write_at of ring/0 failed with OS error 5"
            );
        }
    }
}
