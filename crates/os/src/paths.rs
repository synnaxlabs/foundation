//! The files of the host by the paths that a user gave.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use env::entropy::Driver as _;
use env::paths::{Error, Found, Key, names_directory};
use rustix::io::Errno;

use crate::{dir, entropy, host};

#[derive(Debug)]
pub(crate) struct Driver;

impl env::paths::Driver for Driver {
    fn read(&self, path: &Path, most: usize) -> Result<Found, Error> {
        read(path, most).map_err(|e| error(&e))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        write(path, bytes).map_err(|e| error(&e))
    }

    fn make_dir(&self, path: &Path) -> Result<(), Error> {
        make_dir(path).map_err(|e| error(&e))
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf, Error> {
        resolve(path).map_err(|e| error(&e))
    }

    fn home(&self) -> Option<PathBuf> {
        resolve(&std::env::home_dir()?).ok()
    }

    fn key(&self, path: &Path) -> Result<Key, Error> {
        let file = fs::metadata(path).map_err(|e| error(&e))?;
        Ok(Key::new(file.dev(), file.ino()))
    }
}

fn resolve(path: &Path) -> io::Result<PathBuf> {
    let missing = match fs::canonicalize(path) {
        // macOS resolves a file at a path that ends in `/`, `/.`, or `/..`, also
        // through a link, where the stat fails. The stat also fails past the longest
        // path that the host takes, where glibc `realpath` does not, which bounds the
        // walk of `Paths::within`.
        Ok(resolved) => return fs::metadata(path).map(|_| resolved),
        // Only a last name that is not there passes `symlink_metadata` below.
        Err(e) => e,
    };
    // `file_name` is `None` for an end in `..`, whose directory must be there.
    let (Some(name), Some(dir)) = (path.file_name(), path.parent()) else {
        return Err(missing);
    };
    // Not `path`: an end in `/` or `/.` makes the stat follow a link at `name`.
    match fs::symlink_metadata(dir.join(name)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        // A symbolic link to nothing.
        Ok(_) => return Err(missing),
        Err(e) => return Err(e),
    }
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    Ok(fs::canonicalize(dir)?.join(name))
}

fn read(path: &Path, most: usize) -> io::Result<Found> {
    let file = match host::open(path)? {
        host::Readable::Regular(file, _) => file,
        host::Readable::Directory(dir) => {
            let (mut entries, mut size) = (Vec::new(), 0_usize);
            for name in dir::names(dir)? {
                let entry = path.join(name?);
                size = size.saturating_add(entry.as_os_str().len());
                if size > most {
                    return Ok(Found::Large);
                }
                entries.push(entry);
            }
            entries.sort_unstable();
            return Ok(Found::Directory(entries));
        }
    };
    let over = u64::try_from(most).map_or(u64::MAX, |most| most.saturating_add(1));
    let mut bytes = Vec::new();
    File::from(file).take(over).read_to_end(&mut bytes)?;
    if bytes.len() > most {
        return Ok(Found::Large);
    }
    Ok(Found::File(bytes))
}

fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // `Path` drops an end in `/` or `.`, and takes a name after an end in `..`, so the
    // hidden file would go beside that directory or in it. The stat does not stop
    // this: the directory can come after it. Such a path resolves only to a
    // directory, so the open fails.
    if names_directory(path) {
        return in_place(path, bytes);
    }
    match fs::symlink_metadata(path) {
        Ok(old) if old.is_file() => {}
        Ok(_) => return in_place(path, bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut key = [0; 8];
    entropy::Driver.fill(&mut key);
    let name = format!(".{:016x}.new", u64::from_le_bytes(key));
    let new = path.with_file_name(name);
    fill(open(&new)?, bytes)
        .and_then(|()| fs::rename(&new, path))
        .or_else(|e| fs::remove_file(&new).and(Err(e)))
}

/// Makes a directory at `path` as [`dir::make`] does.
fn make_dir(path: &Path) -> io::Result<()> {
    // macOS makes the target of a link to nothing at a path that ends in `/`, and
    // Linux gives `EEXIST`.
    let bytes = path.as_os_str().as_bytes();
    let end = bytes
        .iter()
        .rposition(|&byte| byte != b'/')
        .map_or(1, |at| at + 1);
    let path = Path::new(OsStr::from_bytes(bytes.get(..end).unwrap_or(bytes)));
    dir::make(rustix::fs::CWD, path).map_err(io::Error::from)
}

/// Makes a file at `path` whose permission bits give only this user access.
fn open(path: &Path) -> io::Result<File> {
    #[expect(clippy::disallowed_methods, reason = "os writes the user's files")]
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Writes `bytes` through an open of `path`, for a link, a device, a pipe, or a
/// directory, which gives `EISDIR`. A device may refuse a sync, so there is none.
fn in_place(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[expect(clippy::disallowed_methods, reason = "os writes the user's files")]
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(bytes)
}

/// Writes `bytes` to `file`, synced, so a power cut after the rename leaves the
/// whole file.
fn fill(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

fn error(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::NotFound {
        return Error::NotFound;
    }
    // Std gives no code for a path with a nul byte.
    let code = e.raw_os_error().unwrap_or(Errno::INVAL.raw_os_error());
    Error::Io { code }
}
