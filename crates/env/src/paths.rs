//! The files of the host by the paths that a user gave.

use std::fmt::Debug;
use std::path::{Component, Path, PathBuf, is_separator};
use std::rc::Rc;

/// The files of the host by the paths that a user gave, as on a command line. A
/// relative path is relative to the working directory of the process. A path that
/// ends in `/`, `.`, or `..` names a directory, and the empty path names nothing.
/// Each call blocks its thread until the OS answers, and so delays each task of that
/// thread: a shard of a node never calls one, and the files of a node are
/// [`crate::files`]. A handle from [`Paths::within`] reaches only the paths under its
/// folders.
///
/// ```
/// use std::path::Path;
///
/// use env::paths::{Error, Found, Paths};
///
/// fn plan(paths: &Paths) -> Result<Found, Error> {
///     paths.read(Path::new("plant.hcl"), 1 << 24)
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Paths {
    driver: Rc<dyn Driver>,
    /// The limits of each call of `within` that made this handle: a path passes each.
    limits: Rc<[Limit]>,
}

impl Paths {
    /// A handle over `driver`. Only `os` and `sim` call it.
    ///
    /// ```
    /// fn wrap(driver: impl env::paths::Driver + 'static) -> env::paths::Paths {
    ///     env::paths::Paths::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self {
            driver: Rc::new(driver),
            limits: Rc::new([]),
        }
    }

    /// The same files, where each call on a path that does not resolve under one of
    /// `folders` fails with [`Error::Outside`], and one on a path under the folders of
    /// each limit that resolves under one of `hidden` fails with [`Error::Hidden`].
    /// Such a call reaches no file. Each folder is resolved as [`Paths::resolve`] gives
    /// it, and a path is under a folder when the folder is the file at the path or a
    /// directory that holds it, by the file and not by its name: a name in other case
    /// on a file system that ignores case, and a bind mount, reach the same folder. A
    /// folder of `folders` holds a path only while the file at its resolved path is the
    /// file that was there at this call, so one that a program renames away, replaces,
    /// or makes again holds nothing until a new call of `within`. A write replaces a
    /// file, so a folder that is a file holds nothing after a write to it, also through
    /// this handle. Each call takes the file at each hidden folder again, from the path
    /// that the caller gave, so a hidden file that a write replaces, and a hidden
    /// folder made after this call, are hidden. A path also fails with
    /// [`Error::Hidden`] when a directory on its way is the folder that was at a hidden
    /// path at this call, so a hidden folder that a program renames away stays hidden,
    /// but not its own listing, and a directory that the host gives the key of a hidden
    /// folder that it removed hides what it holds. While nothing is at a hidden path, a
    /// call that makes a new name in the deepest directory on its way that is there,
    /// when that directory is under a folder of each limit, fails with
    /// [`Error::Hidden`], also for a name that does not make that path. A folder whose
    /// file a call cannot take holds nothing at that call. The limits hold on each step
    /// of a path: each prefix of its components that resolves, in order and without an
    /// end `/` or `/.`, must pass them, and the first that fails gives its error. A
    /// prefix short of the whole path is checked against the folders only when a `..`
    /// after it takes the path above it, or nothing is there, and then also passes when
    /// it is a directory on the way to a folder. A step meets `hidden` only when it
    /// passes the folders of each limit. The name of the first prefix that does not
    /// resolve must pass them too, at its place with no key, as a new name there does.
    /// A relative path starts at the working directory: the steps of that directory
    /// come first, and each prefix after them is a prefix of the path as given. A path
    /// whose steps pass and that does not resolve gives the error of its resolve. The
    /// limits of `self` hold too, so a handle never reaches more than the one it came
    /// from. The check and the call are two steps: a symbolic link that another program
    /// makes between them can lead the call out.
    ///
    /// # Errors
    ///
    /// The error of [`Paths::resolve`] for a path of `folders` or `hidden`, or of
    /// [`Driver::key`] for a path of `folders`, and for a path of `hidden` other than
    /// [`Error::NotFound`]: [`Error::NotFound`] when a folder of `folders` is not
    /// there.
    ///
    /// ```
    /// use std::path::PathBuf;
    ///
    /// use env::paths::{Error, Paths};
    ///
    /// fn tools(paths: &Paths, work: PathBuf, data: PathBuf) -> Result<Paths, Error> {
    ///     paths.within(&[work], &[data])
    /// }
    /// ```
    pub fn within(
        &self,
        folders: &[PathBuf],
        hidden: &[PathBuf],
    ) -> Result<Self, Error> {
        let mut limit = Limit {
            folders: Vec::new(),
            hidden: Vec::new(),
        };
        for dir in folders {
            let dir = self.driver.resolve(dir)?;
            let key = self.driver.key(&dir)?;
            limit.folders.push((dir, key));
        }
        for given in hidden {
            let key = self.key(given)?;
            limit.hidden.push(Hidden {
                given: given.clone(),
                resolved: self.driver.resolve(given)?,
                was: key,
            });
        }
        Ok(Self {
            driver: Rc::clone(&self.driver),
            limits: self.limits.iter().cloned().chain([limit]).collect(),
        })
    }

    /// The absolute path that `path` names, with each `.`, `..`, and symbolic link
    /// resolved. When nothing is at the last name of `path`, it keeps that name on
    /// its resolved directory. It reads no file and checks no limit of
    /// [`Paths::within`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `path` is empty, when a directory on the way is not
    /// there, or when a symbolic link at its last name names nothing, else
    /// [`Error::Io`].
    ///
    /// ```
    /// use std::path::{Path, PathBuf};
    ///
    /// use env::paths::{Error, Paths};
    ///
    /// fn working(paths: &Paths) -> Result<PathBuf, Error> {
    ///     paths.resolve(Path::new("."))
    /// }
    /// ```
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, Error> {
        self.driver.resolve(path)
    }

    /// The home directory of the user, resolved as [`Paths::resolve`] gives it, or
    /// `None` when the host gives none or it does not resolve.
    ///
    /// ```
    /// fn home(paths: &env::paths::Paths) -> Option<std::path::PathBuf> {
    ///     paths.home()
    /// }
    /// ```
    #[must_use]
    pub fn home(&self) -> Option<PathBuf> {
        self.driver.home()
    }

    /// What is at `path`: the bytes of a file of at most `most` bytes, or the path of
    /// each entry of a directory whose entry paths hold at most `most` bytes in all, in
    /// name order. It reads through one open of `path`: a later rename or replace at
    /// `path` does not change what it reads.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when nothing is at `path`, [`Error::Outside`] or
    /// [`Error::Hidden`] past the limits of [`Paths::within`], [`Error::Io`] 22 when
    /// `path` is a file that is neither a regular file nor a directory, such as a FIFO
    /// or a device, else [`Error::Io`]. When another writer changes a link in `path`
    /// during the call, it gives the error of the open of the file that the link then
    /// names, and opens that file, also a device. On Linux, the walk of a link that a
    /// rename replaces during the call can read only a first part of its old target,
    /// also none of it, so the call can open the folder of the link, or a folder on the
    /// way to that target, such as `/`, and the rest of `path` resolves under that
    /// folder. When the link is the last part of `path`, the read gives the entries of
    /// that folder.
    ///
    /// ```
    /// use std::path::{Path, PathBuf};
    ///
    /// use env::paths::{Found, Paths};
    ///
    /// fn entries(paths: &Paths) -> Vec<PathBuf> {
    ///     match paths.read(Path::new("plans"), 1 << 16) {
    ///         Ok(Found::Directory(entries)) => entries,
    ///         _ => Vec::new(),
    ///     }
    /// }
    /// ```
    pub fn read(&self, path: &Path, most: usize) -> Result<Found, Error> {
        self.check(path)??;
        self.driver.read(path, most)
    }

    /// Writes `bytes` as the file at `path`, and replaces a file that is there. The
    /// write is whole or not at all when a file or nothing is at `path`. A symbolic
    /// link, a device, or a pipe at `path` takes the bytes in place: a reader, a crash,
    /// or a failed write can see or leave a part of them, and two writes at one time
    /// can leave a mix of the two.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the directory of `path`, the directory that it names,
    /// or the file that a symbolic link at `path` names is not there,
    /// [`Error::Outside`] or [`Error::Hidden`] past the limits of [`Paths::within`],
    /// else [`Error::Io`].
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// use env::paths::{Error, Paths};
    ///
    /// fn out(paths: &Paths, plan: &[u8]) -> Result<(), Error> {
    ///     paths.write(Path::new("plant.plan"), plan)
    /// }
    /// ```
    pub fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        self.check(path)??;
        self.driver.write(path, bytes)
    }

    /// Makes the directory at `path`. Gives `Ok` when a directory is there already. It
    /// makes no directory above it, and none through a symbolic link at `path`. A
    /// directory that it makes gives the group and other users no access. On a handle
    /// of [`Paths::within`], `path` first passes the limits of [`Paths::within`]. When
    /// `path` does not resolve and the directory is made, the call checks `path` again
    /// and gives the error of that check.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the directory above `path` is not there,
    /// [`Error::Outside`] or [`Error::Hidden`] past the limits of [`Paths::within`],
    /// else [`Error::Io`], also when a file or a link to nothing is at `path`.
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// use env::paths::{Error, Paths};
    ///
    /// fn data(paths: &Paths) -> Result<(), Error> {
    ///     paths.make_dir(Path::new("data"))
    /// }
    /// ```
    pub fn make_dir(&self, path: &Path) -> Result<(), Error> {
        match self.check(path)? {
            // A link that does not resolve can lead to a directory that its place
            // does not hold. A directory made there resolves.
            Err(_) => {
                self.driver.make_dir(path)?;
                self.check(path)?
            }
            Ok(()) => self.driver.make_dir(path),
        }
    }

    /// `Ok` with the result of the resolve of `path` when each step of it passes each
    /// limit: each prefix of the resolved working directory first for a relative path,
    /// then each prefix of its components that resolves, in order, where a step short
    /// of `path` meets the folders as its [`Way`] gives, then the name of the first
    /// prefix that does not resolve, at its place with no key. Else the error of the
    /// first step that fails.
    fn check(&self, path: &Path) -> Result<Result<(), Error>, Error> {
        if self.limits.is_empty() {
            return Ok(Ok(()));
        }
        let mut parts: Vec<_> = path.components().collect();
        if parts.first() == Some(&Component::CurDir) {
            parts.remove(0);
        }
        let mut part = PathBuf::new();
        let mut above = PathBuf::new();
        // `up[index]` is how many directories above the place before `parts[index]`
        // the rest of the path goes.
        let mut up = vec![0_usize; parts.len() + 1];
        let mut count = 0_usize;
        for (index, step) in parts.iter().enumerate().rev() {
            match step {
                Component::ParentDir => count += 1,
                Component::Normal(_) => count = count.saturating_sub(1),
                _ => {}
            }
            up[index] = count;
        }
        // Each step is a prefix of the resolved working directory or of `path`, never
        // of their join, which can be longer than the host takes.
        if path.is_relative() && !path.as_os_str().is_empty() {
            let working = self.driver.resolve(Path::new("."))?;
            let mut below = working.components().count();
            let mut names = working.components().peekable();
            while let Some(name) = names.next() {
                above.push(name);
                below -= 1;
                let short = names.peek().is_some() || !parts.is_empty();
                let back = up[0] > below;
                self.allowed(self.key(&above)?, &above, Way::of(short, back))?;
            }
        }
        for (index, step) in parts.iter().enumerate() {
            part.push(step);
            let short = index + 1 < parts.len();
            let Ok(resolved) = self.driver.resolve(&part) else {
                if let Component::Normal(name) = step {
                    self.allowed(None, &above.join(name), Way::of(short, true))?;
                }
                break;
            };
            let back = up[index + 1] > 0;
            self.allowed(self.key(&part)?, &resolved, Way::of(short, back))?;
            above = resolved;
        }
        Ok(self.driver.resolve(path).map(|_| ()))
    }

    /// `Ok` when the file at `resolved` passes each limit. `own` is the key that the
    /// host gives for the name that the call reaches, which for a link is the key of
    /// the file that it names, and `None` for a name with no key. The caller takes it
    /// from that name, since the resolved path of a new name can be longer than the
    /// host takes. `way` gives what the folders ask of it.
    fn allowed(
        &self,
        own: Option<Key>,
        resolved: &Path,
        way: Way,
    ) -> Result<(), Error> {
        let mut dirs = Vec::new();
        for dir in resolved.ancestors().skip(1) {
            dirs.push(self.driver.key(dir)?);
        }
        let keys: Vec<_> = own.into_iter().chain(dirs.iter().copied()).collect();
        let held = |(dir, was): &(PathBuf, Key)| {
            keys.contains(was) && self.driver.key(dir).ok() == Some(*was)
        };
        let above = |(dir, _): &(PathBuf, Key)| {
            own.is_some_and(|own| {
                dir.ancestors()
                    .skip(1)
                    .any(|up| self.driver.key(up).ok() == Some(own))
            })
        };
        let passes = |folder| held(folder) || way == Way::Short && above(folder);
        let inside = || {
            self.limits
                .iter()
                .all(|limit| limit.folders.iter().any(passes))
        };
        let down = way == Way::Down && own.is_some();
        if !down && !inside() {
            return Err(Error::Outside);
        }
        for limit in self.limits.iter() {
            for Hidden {
                given,
                resolved,
                was,
            } in &limit.hidden
            {
                let moved = was.is_some_and(|key| dirs.contains(&key));
                let hidden = match self.key(given) {
                    Ok(Some(key)) => keys.contains(&key),
                    Ok(None) => {
                        own.is_none() && dirs.first() == self.above(resolved).as_ref()
                    }
                    Err(_) => false,
                };
                // Only a step that the folders of each limit hold meets the hidden
                // folders: elsewhere they would tell what is outside the folders.
                if (moved || hidden) && (!down || inside()) {
                    return Err(Error::Hidden);
                }
            }
        }
        Ok(())
    }

    /// The key of the deepest directory above `dir` that is there.
    fn above(&self, dir: &Path) -> Option<Key> {
        dir.ancestors()
            .skip(1)
            .find_map(|dir| self.driver.key(dir).ok())
    }

    /// The key of the file at `dir`, or `None` when nothing is there.
    fn key(&self, dir: &Path) -> Result<Option<Key>, Error> {
        match self.driver.key(dir) {
            Ok(key) => Ok(Some(key)),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// A hidden folder of one call of [`Paths::within`].
#[derive(Clone, Debug)]
struct Hidden {
    /// The path that the caller gave. Each call takes its key from it, as the call
    /// reaches it: the resolved path of a new name can be longer than the host takes.
    given: PathBuf,
    resolved: PathBuf,
    /// The key of what was there at `within`.
    was: Option<Key>,
}

/// What the folders ask of a step of the walk of [`Paths::within`].
#[derive(Clone, Copy, PartialEq)]
enum Way {
    /// The step ends the walk: a folder holds it.
    End,
    /// A `..` after the step takes the path above it: a folder holds it, or it is a
    /// directory above one.
    Short,
    /// No `..` takes the path above the step, so the step changes where the path ends
    /// only as a link or a mount, and the end meets the folders for it, when it is
    /// there.
    Down,
}

impl Way {
    fn of(short: bool, back: bool) -> Self {
        match (short, back) {
            (false, _) => Self::End,
            (true, true) => Self::Short,
            (true, false) => Self::Down,
        }
    }
}

/// The folders of one call of [`Paths::within`].
#[derive(Clone, Debug)]
struct Limit {
    /// The resolved path of each folder, with the key of what was there.
    folders: Vec<(PathBuf, Key)>,
    hidden: Vec<Hidden>,
}

/// The key of a file of the host: two paths that name the same file have the same
/// key, also through a bind mount or a name in other case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    device: u64,
    inode: u64,
}

impl Key {
    /// The key of file `inode` on the file system `device`. Only `os` and `sim` call
    /// it.
    ///
    /// ```
    /// assert_ne!(env::paths::Key::new(1, 2), env::paths::Key::new(2, 1));
    /// ```
    #[must_use]
    pub const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }
}

/// What [`Paths::read`] found at a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// A file of at most `most` bytes: its bytes.
    File(Vec<u8>),
    /// A file of more than `most` bytes, or a directory whose entry paths hold more.
    Large,
    /// A directory: the path of each entry, in name order.
    Directory(Vec<PathBuf>),
}

/// Why a call of [`Paths`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Nothing is at the path, or the directory or the link target that a write needs
    /// is not there.
    NotFound,
    /// The OS refused the call with `code`.
    Io {
        /// The OS error code.
        code: i32,
    },
    /// The path resolves outside the folders of [`Paths::within`].
    Outside,
    /// The path resolves under a hidden folder of [`Paths::within`].
    Hidden,
}

/// What `os` and `sim` implement: the calls of [`Paths`], with the same contracts.
///
/// ```
/// fn wrap(driver: impl env::paths::Driver + 'static) -> env::paths::Paths {
///     env::paths::Paths::new(driver)
/// }
/// ```
pub trait Driver: Debug {
    /// [`Paths::read`].
    ///
    /// # Errors
    ///
    /// As [`Paths::read`].
    fn read(&self, path: &Path, most: usize) -> Result<Found, Error>;

    /// [`Paths::write`].
    ///
    /// # Errors
    ///
    /// As [`Paths::write`].
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), Error>;

    /// [`Paths::make_dir`].
    ///
    /// # Errors
    ///
    /// As [`Paths::make_dir`].
    fn make_dir(&self, path: &Path) -> Result<(), Error>;

    /// [`Paths::resolve`].
    ///
    /// # Errors
    ///
    /// As [`Paths::resolve`].
    fn resolve(&self, path: &Path) -> Result<PathBuf, Error>;

    /// [`Paths::home`].
    fn home(&self) -> Option<PathBuf>;

    /// The key of the file at `path`, after each symbolic link.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when nothing is at `path`, else [`Error::Io`].
    fn key(&self, path: &Path) -> Result<Key, Error>;
}

/// Whether `path` is empty or ends in a separator, `.`, or `..`. By its text, such a
/// path names a directory or nothing, never a file. It reads no file. A separator is a
/// `/`, and on Windows also a `\` (`std::path::is_separator`). The methods of `Path` do
/// not give this: they drop an end in a separator or `.`.
#[must_use]
pub fn names_directory(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    // A separator is ASCII, and no byte of a longer character is ASCII in these bytes.
    let name = bytes.rsplit(|&byte| is_separator(char::from(byte))).next();
    matches!(name, Some(b"" | b"." | b".."))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// One file, at any path but one under `/none`. A path is under `/`, and `link`
    /// names `/out`. A directory that it makes is the file `dir`. A name in `/none`
    /// resolves only after a directory is made, and `/none/deep`, a link to a directory
    /// that does not resolve, and each path in it, never. The key of a path ignores
    /// case, and a last name that starts with `new` resolves but has no key yet. The
    /// key of a last name that starts with `swap` in any case takes the bytes of the
    /// file after the path, so a write gives it a new key: after a write of `x`,
    /// `/work/swap` has the key that `/work/swapx` had before it. A last name `io`
    /// gives `Io` 5 for its key. A path in `/volume` has the key of the path without
    /// `/volume`, as a second mount of `/` does. It counts its calls of `resolve` and
    /// `key`, and a relative path is under its working directory, `/` at first.
    #[derive(Debug, Default)]
    struct One(RefCell<Vec<u8>>, Rc<Cell<usize>>, Rc<RefCell<PathBuf>>);

    impl Driver for One {
        fn read(&self, path: &Path, most: usize) -> Result<Found, Error> {
            match path.to_str() {
                Some(path) if path.is_empty() || path.starts_with("/none") => {
                    Err(Error::NotFound)
                }
                Some("dir") => Ok(Found::Directory(vec![path.join("a")])),
                _ if self.0.borrow().len() > most => Ok(Found::Large),
                _ => Ok(Found::File(self.0.borrow().clone())),
            }
        }

        fn write(&self, _: &Path, bytes: &[u8]) -> Result<(), Error> {
            if bytes.is_empty() {
                return Err(Error::Io { code: 28 });
            }
            *self.0.borrow_mut() = bytes.to_vec();
            Ok(())
        }

        fn make_dir(&self, path: &Path) -> Result<(), Error> {
            self.write(path, b"dir")
        }

        fn resolve(&self, path: &Path) -> Result<PathBuf, Error> {
            self.1.set(self.1.get() + 1);
            match path.strip_prefix("link") {
                _ if path.as_os_str().is_empty() => Err(Error::NotFound),
                _ if path.starts_with("/none/deep")
                    || path.parent() == Some(Path::new("/none"))
                        && *self.0.borrow() != b"dir" =>
                {
                    Err(Error::NotFound)
                }
                Ok(rest) => Ok(Path::new("/out").join(rest)),
                Err(_) => {
                    let at = Path::new("/").join(&*self.2.borrow()).join(path);
                    Ok(at.components().collect())
                }
            }
        }

        fn home(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/user"))
        }

        fn key(&self, path: &Path) -> Result<Key, Error> {
            self.1.set(self.1.get() + 1);
            let name = path.file_name().unwrap_or_default().as_encoded_bytes();
            if name.starts_with(b"new") || path.ends_with("/none/deep") {
                return Err(Error::NotFound);
            }
            let mut at = self.resolve(path)?;
            if let Ok(rest) = at.strip_prefix("/volume")
                && rest != Path::new("")
            {
                at = Path::new("/").join(rest).components().collect();
            }
            // FNV-1a.
            if name == b"io" {
                return Err(Error::Io { code: 5 });
            }
            let mut folded = at.as_os_str().as_encoded_bytes().to_ascii_lowercase();
            if name.to_ascii_lowercase().starts_with(b"swap") {
                folded.extend_from_slice(&self.0.borrow());
            }
            let hash = folded
                .iter()
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
                });
            Ok(Key::new(0, hash))
        }
    }

    /// What a write, a read, and a `make_dir` of `path` give, which agree.
    fn reached(paths: &Paths, path: &str) -> Result<(), Error> {
        let path = Path::new(path);
        let written = paths.write(path, b"x");
        let read = paths.read(path, 1).map(|_| ());
        let made = paths.make_dir(path);
        assert_eq!([&written, &read], [&made; 2], "{}", path.display());
        written
    }

    #[test]
    fn calls_reach_the_driver_with_their_arguments() {
        let paths = Paths::new(One::default());
        let clone = paths.clone();
        assert_eq!(paths.write(Path::new("a"), b"xyz"), Ok(()));
        assert_eq!(
            clone.read(Path::new("a"), 3),
            Ok(Found::File(b"xyz".to_vec()))
        );
        assert_eq!(clone.read(Path::new("a"), 2), Ok(Found::Large));
        let entries = Found::Directory(vec![PathBuf::from("dir/a")]);
        assert_eq!(paths.read(Path::new("dir"), 0), Ok(entries));
        assert_eq!(paths.read(Path::new(""), 0), Err(Error::NotFound));
        assert_eq!(
            paths.write(Path::new("a"), b""),
            Err(Error::Io { code: 28 })
        );
        assert_eq!(paths.make_dir(Path::new("b")), Ok(()));
        assert_eq!(
            paths.read(Path::new("b"), 3),
            Ok(Found::File(b"dir".to_vec()))
        );
    }

    #[test]
    fn resolve_and_home_reach_the_driver_and_check_no_limit() {
        let paths = Paths::new(One::default())
            .within(&["/work".into()], &[])
            .unwrap();
        let resolved = paths.resolve(Path::new("link/a"));
        assert_eq!(resolved, Ok(PathBuf::from("/out/a")));
        assert_eq!(paths.resolve(Path::new("")), Err(Error::NotFound));
        assert_eq!(paths.home(), Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn within_reaches_only_paths_that_resolve_under_a_folder_and_no_hidden_one() {
        let free = Paths::new(One::default());
        let folders = ["work".into(), "/plans".into()];
        let paths = free.within(&folders, &["work/data".into()]).unwrap();
        for path in ["/work", "work/a", "/work/b/c", "/plans/a", "/work/data2"] {
            assert_eq!(reached(&paths, path), Ok(()), "{path}");
        }
        for path in ["/", "/out", "link", "link/a", "/workshop"] {
            assert_eq!(reached(&paths, path), Err(Error::Outside), "{path}");
            assert_eq!(reached(&free, path), Ok(()), "{path}");
        }
        for path in ["/work/data", "work/data/key"] {
            assert_eq!(reached(&paths, path), Err(Error::Hidden), "{path}");
            assert_eq!(reached(&free, path), Ok(()), "{path}");
        }
        assert_eq!(reached(&paths.clone(), "/out"), Err(Error::Outside));
        assert_eq!(paths.read(Path::new(""), 1), Err(Error::NotFound));
        assert_eq!(paths.write(Path::new(""), b"x"), Err(Error::NotFound));
    }

    #[test]
    fn a_folder_that_does_not_resolve_gives_its_error() {
        let free = Paths::new(One::default());
        let empty = [PathBuf::new()];
        assert_eq!(free.within(&empty, &[]).err(), Some(Error::NotFound));
        let folders = ["/work".into()];
        assert_eq!(free.within(&folders, &empty).err(), Some(Error::NotFound));
    }

    #[test]
    fn make_dir_checks_a_path_that_does_not_resolve_by_its_place() {
        // A new `One` for each path: a made directory makes each name in `/none`
        // resolve.
        let within = |folder: &str| {
            let free = Paths::new(One::default());
            free.within(&[folder.into()], &[]).unwrap()
        };
        assert_eq!(
            within("/none").make_dir(Path::new("/none/deep")),
            Err(Error::NotFound)
        );
        assert_eq!(within("/none").make_dir(Path::new("/none/link")), Ok(()));
        for path in ["/none/a", "/none/deep"] {
            let made = within("/work").make_dir(Path::new(path));
            assert_eq!(made, Err(Error::Outside), "{path}");
        }
    }

    #[test]
    fn a_folder_that_is_not_there_is_not_found_and_a_hidden_one_hides_its_name() {
        let free = Paths::new(One::default());
        let none = ["/none/a".into()];
        assert_eq!(
            free.within(&["/work".into()], &none).err(),
            Some(Error::NotFound)
        );
        assert_eq!(free.within(&none, &[]).err(), Some(Error::NotFound));
        let new = ["/work/new".into()];
        assert_eq!(free.within(&new, &[]).err(), Some(Error::NotFound));
        let work = ["/work".into()];
        let paths = free.within(&work, &new).unwrap();
        for path in ["/work/new", "/work/new2"] {
            assert_eq!(reached(&paths, path), Err(Error::Hidden), "{path}");
        }
        assert_eq!(reached(&paths, "/work/a"), Ok(()));
        assert_eq!(reached(&paths, "/work/b/new"), Ok(()));
        let limited = free.within(&work, &[]).unwrap();
        let refused = ["/other/new".into()];
        assert_eq!(limited.within(&work, &refused).err(), None);
        let paths = free
            .within(&["/none".into()], &["/work/data".into()])
            .unwrap();
        assert_eq!(paths.read(Path::new("/none/a"), 1), Err(Error::NotFound));
    }

    #[test]
    fn within_gives_the_error_of_a_hidden_key_other_than_not_found() {
        let free = Paths::new(One::default());
        let within = free.within(&["/work".into()], &["/work/io".into()]);
        assert_eq!(within.err(), Some(Error::Io { code: 5 }));
    }

    #[test]
    fn a_hidden_file_that_a_write_replaces_hides_by_its_new_key() {
        let free = Paths::new(One::default());
        let paths = free
            .within(&["/work".into()], &["/work/swap".into()])
            .unwrap();
        free.write(Path::new("/work/a"), b"y").unwrap();
        assert_eq!(reached(&paths, "/WORK/SWAP"), Err(Error::Hidden));
    }

    #[test]
    fn a_hidden_folder_that_moves_hides_the_files_under_it_but_not_itself() {
        let free = Paths::new(One::default());
        let paths = free
            .within(&["/work".into()], &["/work/swapx".into()])
            .unwrap();
        free.write(Path::new("/work/a"), b"x").unwrap();
        assert_eq!(reached(&paths, "/work/swap/a"), Err(Error::Hidden));
        assert_eq!(
            paths.read(Path::new("/work/swap"), 1),
            Ok(Found::File(b"x".into()))
        );
    }

    #[test]
    fn a_folder_that_is_replaced_holds_nothing() {
        let free = Paths::new(One::default());
        let paths = free.within(&["/work/swapx".into()], &[]).unwrap();
        free.write(Path::new("/work/a"), b"x").unwrap();
        assert_eq!(reached(&paths, "/work/swap/a"), Err(Error::Outside));
        assert_eq!(reached(&paths, "/work/swapx/a"), Err(Error::Outside));
    }

    #[test]
    fn a_hidden_folder_that_the_limits_refuse_and_that_is_there_hides() {
        let free = Paths::new(One::default());
        let outer = free.within(&["/a/b".into()], &[]).unwrap();
        let inner = outer.within(&["/a/b/c".into()], &["/a".into()]).unwrap();
        assert_eq!(reached(&inner, "/a/b/c/d"), Err(Error::Hidden));
        assert_eq!(reached(&outer, "/a/b/c/d"), Ok(()));
    }

    #[test]
    fn a_folder_is_its_file_and_not_its_name() {
        let free = Paths::new(One::default());
        let paths = free
            .within(&["/work".into()], &["/work/data".into()])
            .unwrap();
        for path in ["/WORK", "/Work/a", "/WORK/b/c", "/work/new", "/WORK/DATA2"] {
            assert_eq!(reached(&paths, path), Ok(()), "{path}");
        }
        for path in ["/WORK/DATA", "/work/Data/key", "/Work/data/new"] {
            assert_eq!(reached(&paths, path), Err(Error::Hidden), "{path}");
        }
        assert_eq!(reached(&paths, "/WORKSHOP"), Err(Error::Outside));
    }

    #[test]
    fn a_step_before_a_dot_dot_passes_on_the_way_to_a_folder() {
        let paths = Paths::new(One::default())
            .within(&["/work".into()], &[])
            .unwrap();
        assert_eq!(reached(&paths, "/work/../work/a"), Ok(()));
    }

    #[test]
    fn a_folder_holds_its_files_through_another_mount() {
        let one = One::default();
        let working = Rc::clone(&one.2);
        let paths = Paths::new(one)
            .within(&["/work".into()], &["/work/data".into()])
            .unwrap();
        assert_eq!(reached(&paths, "/volume/work/a"), Ok(()));
        assert_eq!(reached(&paths, "/volume/work/data/key"), Err(Error::Hidden));
        assert_eq!(reached(&paths, "/volume/out"), Err(Error::Outside));
        *working.borrow_mut() = PathBuf::from("/volume/work");
        assert_eq!(reached(&paths, "a"), Ok(()));
        assert_eq!(reached(&paths, "data/key"), Err(Error::Hidden));
        *working.borrow_mut() = PathBuf::from("/volume/out");
        assert_eq!(reached(&paths, "a"), Err(Error::Outside));
    }

    #[test]
    fn a_hidden_folder_meets_only_a_step_inside_the_folders() {
        let one = One::default();
        let working = Rc::clone(&one.2);
        let hidden = ["/work/data".into(), "/out/h".into()];
        let paths = Paths::new(one).within(&["/work".into()], &hidden).unwrap();
        assert_eq!(reached(&paths, "/out/h/a"), Err(Error::Outside));
        *working.borrow_mut() = PathBuf::from("/work/data");
        assert_eq!(reached(&paths, "link/a"), Err(Error::Hidden));
    }

    #[test]
    fn a_dot_dot_that_stays_in_another_mount_of_a_folder_passes() {
        let one = One::default();
        let working = Rc::clone(&one.2);
        let paths = Paths::new(one).within(&["/work".into()], &[]).unwrap();
        assert_eq!(reached(&paths, "/volume/work/x/../a"), Ok(()));
        let left = reached(&paths, "/volume/work/s/../../../work/a");
        assert_eq!(left, Err(Error::Outside));
        *working.borrow_mut() = PathBuf::from("/volume/work/s");
        assert_eq!(reached(&paths, "../a"), Ok(()));
        assert_eq!(reached(&paths, "x/../../a"), Ok(()));
        assert_eq!(reached(&paths, "../../work/a"), Ok(()));
        assert_eq!(reached(&paths, "../../../a"), Err(Error::Outside));
    }

    #[test]
    fn a_directory_on_the_way_with_no_key_gives_its_error() {
        let paths = Paths::new(One::default())
            .within(&["/".into()], &[])
            .unwrap();
        assert_eq!(reached(&paths, "/none/deep/a"), Err(Error::NotFound));
        assert_eq!(reached(&paths, "/work/new/a"), Err(Error::NotFound));
    }

    #[test]
    fn a_step_whose_key_fails_gives_its_error() {
        let paths = Paths::new(One::default())
            .within(&["/".into()], &[])
            .unwrap();
        assert_eq!(reached(&paths, "/work/io"), Err(Error::Io { code: 5 }));
    }

    #[test]
    fn a_path_stops_at_its_first_step_that_does_not_resolve() {
        let one = One::default();
        let calls = Rc::clone(&one.1);
        let paths = Paths::new(one).within(&["/".into()], &[]).unwrap();
        let mut path = String::from("/none/deep");
        for _ in 0..1000 {
            path.push_str("/a");
        }
        calls.set(0);
        assert_eq!(paths.read(Path::new(&path), 1), Err(Error::NotFound));
        // On Windows the walk takes a path with no drive as relative and checks the
        // steps of the working directory first (#3018).
        assert_eq!(calls.get(), if cfg!(windows) { 19 } else { 16 });
    }

    #[test]
    fn a_directory_is_not_made_past_a_step_with_an_error() {
        let free = Paths::new(One::default());
        free.write(Path::new("/a"), b"old").unwrap();
        let paths = free.within(&["/".into()], &[]).unwrap();
        let made = paths.make_dir(Path::new("/work/io/x"));
        assert_eq!(made, Err(Error::Io { code: 5 }));
        assert_eq!(
            free.read(Path::new("/a"), 3),
            Ok(Found::File(b"old".to_vec()))
        );
    }

    #[test]
    fn a_path_outside_reaches_no_file() {
        let free = Paths::new(One::default());
        free.write(Path::new("/out/a"), b"old").unwrap();
        let paths = free.within(&["/work".into()], &[]).unwrap();
        assert_eq!(
            paths.write(Path::new("/out/a"), b"new"),
            Err(Error::Outside)
        );
        assert_eq!(paths.make_dir(Path::new("/out/a")), Err(Error::Outside));
        assert_eq!(
            free.read(Path::new("/out/a"), 3),
            Ok(Found::File(b"old".to_vec()))
        );
    }

    #[test]
    fn a_handle_within_another_keeps_its_limits() {
        let free = Paths::new(One::default());
        let wide = free.within(&["/".into()], &["/work/data".into()]).unwrap();
        let narrow = wide.within(&["/work".into()], &[]).unwrap();
        assert_eq!(reached(&narrow, "/work/a"), Ok(()));
        assert_eq!(reached(&narrow, "/out"), Err(Error::Outside));
        assert_eq!(reached(&narrow, "/work/data"), Err(Error::Hidden));
        let beside = wide.within(&["/plans".into()], &[]).unwrap();
        assert_eq!(reached(&beside, "/work/data"), Err(Error::Outside));
        let widened = narrow.within(&["/".into()], &[]).unwrap();
        assert_eq!(reached(&widened, "/out"), Err(Error::Outside));
        assert_eq!(reached(&wide, "/out"), Ok(()));
    }

    #[test]
    fn a_path_that_is_empty_or_ends_in_a_slash_or_dots_names_a_directory() {
        for path in ["", "/", "a/", "a//", ".", "..", "a/.", "a/.."] {
            assert!(names_directory(Path::new(path)), "{path:?}");
        }
        for path in ["a", "/a", "a/b", ".a", "a.", "..a", "a/..."] {
            assert!(!names_directory(Path::new(path)), "{path:?}");
        }
        for path in ["a\\", "a\\.."] {
            assert_eq!(names_directory(Path::new(path)), cfg!(windows), "{path:?}");
        }
    }
}
