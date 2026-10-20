//! The files of a program's host: the profile, which names the subject and pins the
//! public key of each node, and the private key file of the subject.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;

use config::openssh::{self, Unreadable};
use document::Source;
use document::encoding::Checked;
use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;

/// The profile of the host when `FOUNDATION_PROFILE` names no file.
const DEFAULT: &str = "~/.foundation/profile.hcl";

/// The file of the profile of the host.
pub(crate) struct Profile {
    /// The file that `FOUNDATION_PROFILE` names, else `~/.foundation/profile.hcl`, or
    /// `None` when the user has no home directory for it. An empty
    /// `FOUNDATION_PROFILE` counts as unset.
    pub(crate) path: Option<PathBuf>,
}

impl Profile {
    /// The file of the profile of the host. Reads nothing of it.
    pub(crate) fn host(py: Python<'_>) -> PyResult<Self> {
        let environ = py.import("os")?.getattr("environ")?;
        let named: Option<PathBuf> = environ
            .call_method1("get", ("FOUNDATION_PROFILE",))?
            .extract()?;
        let path = match named.filter(|path| !path.as_os_str().is_empty()) {
            Some(path) => Some(path),
            None => home(py)?.map(|home| home.join(".foundation/profile.hcl")),
        };
        Ok(Self { path })
    }

    /// The path of the file as a message names it: `~/.foundation/profile.hcl` when
    /// the user has no home directory.
    pub(crate) fn shown(&self) -> &Path {
        self.path.as_deref().unwrap_or(Path::new(DEFAULT))
    }

    /// What the file holds, or an empty profile when it is not there or the user has
    /// no home directory for it. Raises `ValueError` for a profile with a problem, with
    /// each problem by line, or that is not UTF-8, and `OSError` when the file cannot
    /// be read.
    pub(crate) fn read(&self) -> PyResult<config::profile::Profile> {
        let text = match &self.path {
            Some(path) => text(path)?.unwrap_or_default(),
            None => String::new(),
        };
        decoded(self.shown(), &text)
    }

    /// The path of the file. Raises `OSError` when the user has no home directory
    /// for it.
    fn file(&self) -> PyResult<&Path> {
        self.path
            .as_deref()
            .ok_or_else(|| homeless(Path::new(DEFAULT)))
    }

    /// Pins `public_key` as the key of the node at `address` in the file, unless a
    /// `node` block of `address` is there, and returns the key that the file pins.
    /// Each other byte of the file stays, and a file that is a link stays one. Two
    /// programs that pin at once take turns under a lock of the file `<path>.lock`,
    /// and the file changes in one rename, which is on the disk before it returns.
    /// Makes each missing directory of the path with mode 0700, and the file with
    /// mode 0600. Raises `ValueError` when the file has a problem, and `OSError` when
    /// it cannot be read or written, or the user has no home directory for it.
    pub(crate) fn pin(
        &self,
        address: SocketAddr,
        public_key: PublicKey,
    ) -> PyResult<PublicKey> {
        let file = self.file()?;
        let os = |e: io::Error| PyOSError::new_err(format!("{}: {e}", file.display()));
        let path = match fs::canonicalize(file) {
            Ok(path) => path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                linked(file).map_err(os)?
            }
            Err(e) => return Err(os(e)),
        };
        private_directory(directory(&path)).map_err(os)?;
        let lock = private_file(
            OpenOptions::new().write(true).create(true),
            &sibling(&path, "lock"),
        )
        .map_err(os)?;
        lock.lock().map_err(os)?;
        let text = text(&path)?.unwrap_or_default();
        if let Some(pinned) = decoded(file, &text)?.nodes.get(&address) {
            return Ok(*pinned);
        }
        let mut document = config_hcl::read(Source(0), &text)
            .unwrap_or_else(|_| unreachable!("invariant: `decoded` read the text"));
        document
            .blocks
            .push(config::profile::node(address, public_key));
        let document = Checked::new(document)
            .unwrap_or_else(|_| unreachable!("invariant: a profile is shallow"));
        let text =
            config_hcl::update(Source(0), &text, &document).unwrap_or_else(|_| {
                unreachable!("invariant: the text reads, and a node block writes")
            });
        let temporary = sibling(&path, "new");
        let mut file = private_file(
            OpenOptions::new().write(true).create(true).truncate(true),
            &temporary,
        )
        .map_err(os)?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &path))
            .and_then(|()| synced(directory(&path)))
            .map_err(os)?;
        Ok(public_key)
    }

    /// The private key of `subject` in the key file `named`, else in
    /// `~/.foundation/<subject>.key`, which [`made`] makes when it is not there. A
    /// `named` path that starts with `~/` is in the home directory, and a relative one
    /// is in the directory of the profile's path, also when the profile is a link.
    /// Raises `ValueError` as [`made`] does, and `OSError` when a file cannot be read
    /// or written, `named` is not there, or the user has no home directory for the
    /// file.
    pub(crate) fn private_key(
        &self,
        py: Python<'_>,
        subject: &Name,
        named: Option<&Path>,
    ) -> PyResult<PrivateKey> {
        let default = Path::new("~/.foundation").join(format!("{subject}.key"));
        let Some(named) = named else {
            let fix = "move the file away, and connect makes a new key there";
            return made(subject, &expanded(py, &default)?, fix);
        };
        let fix = format!(
            "remove `private_key` from {}, and connect makes {}",
            self.shown().display(),
            default.display()
        );
        let path = self.file()?.with_file_name("").join(expanded(py, named)?);
        key(&path, &fix)?.ok_or_else(|| {
            PyOSError::new_err(format!("{}: no file is there: {fix}", path.display()))
        })
    }
}

/// `path` in the home directory when it starts with `~/`, else `path` as written.
/// Raises `OSError` when the user has no home directory.
fn expanded(py: Python<'_>, path: &Path) -> PyResult<PathBuf> {
    let Some(within) = path.to_str().and_then(|path| path.strip_prefix("~/")) else {
        return Ok(path.to_owned());
    };
    let home = home(py)?.ok_or_else(|| homeless(path))?;
    Ok(home.join(within.trim_start_matches('/')))
}

/// The home directory of the user, or `None` when the user has none.
fn home(py: Python<'_>) -> PyResult<Option<PathBuf>> {
    // With no `HOME` and no entry of the uid in the password file, `expanduser` gives
    // `~` as it is.
    let home: PathBuf = py
        .import("os.path")?
        .call_method1("expanduser", ("~",))?
        .extract()?;
    Ok(Some(home).filter(|home| home.is_absolute()))
}

/// The error for `path` in the home directory of a user that has none.
fn homeless(path: &Path) -> PyErr {
    PyOSError::new_err(format!(
        "the user has no home directory for {}: set HOME",
        path.display()
    ))
}

/// The private key of `subject` in the OpenSSH file `path`. When there is no file,
/// makes a new key and writes it there with mode 0600, as `ssh-keygen -t ed25519 -N
/// ""` writes it, and makes each missing directory with mode 0700. The file is on the
/// disk before it returns. Two programs that make it at once get one key. Raises
/// `ValueError` when the file is not the key file of an Ed25519 key with no
/// passphrase, with `fix` for a key with a passphrase, and `OSError` when it cannot be
/// read or written.
fn made(subject: &Name, path: &Path, fix: &str) -> PyResult<PrivateKey> {
    if let Some(key) = key(path, fix)? {
        return Ok(key);
    }
    let os = |e: io::Error| PyOSError::new_err(format!("{}: {e}", path.display()));
    private_directory(directory(path)).map_err(os)?;
    let made = sdk::private_key();
    // A crash can leave the file of this pid.
    let temporary = sibling(path, &format!("{}.new", process::id()));
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            let at = temporary.display();
            return Err(PyOSError::new_err(format!("{at}: {e}")));
        }
        _ => {}
    }
    let mut file =
        private_file(OpenOptions::new().write(true).create_new(true), &temporary)
            .map_err(os)?;
    file.write_all(openssh::key_file(&made, subject.as_str()).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(os)?;
    let linked = fs::hard_link(&temporary, path);
    fs::remove_file(&temporary).map_err(os)?;
    match linked {
        Ok(()) => synced(directory(path)).map_err(os).map(|()| made),
        // Another program made the file first, or `path` is a link to no file.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => key(path, fix)?
            .ok_or_else(|| {
                let at = path.display();
                PyOSError::new_err(format!("{at}: {e}, and it names no file"))
            }),
        Err(e) => Err(os(e)),
    }
}

/// The private key in the OpenSSH file `path`, or `None` when there is no file.
/// Raises as [`bytes`] does, and `ValueError` when it is not the key file of an
/// Ed25519 key with no passphrase, with `fix` for a key with a passphrase.
fn key(path: &Path, fix: &str) -> PyResult<Option<PrivateKey>> {
    bytes(path)?
        .map(|file| openssh::private_key(&file).map_err(|e| unreadable(path, e, fix)))
        .transpose()
}

/// The text of the file `path`, or `None` when there is no file. Raises as [`bytes`]
/// does, and `ValueError` when it is not UTF-8.
fn text(path: &Path) -> PyResult<Option<String>> {
    let Some(bytes) = bytes(path)? else {
        return Ok(None);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(PyValueError::new_err(format!(
            "{}: the file is not UTF-8",
            path.display()
        )));
    };
    Ok(Some(text))
}

/// The bytes of the file `path`, or `None` when there is no file. Raises `OSError`
/// when it cannot be read.
#[expect(
    clippy::disallowed_methods,
    reason = "the files of the program's own host"
)]
fn bytes(path: &Path) -> PyResult<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PyOSError::new_err(format!("{}: {e}", path.display()))),
    }
}

/// The profile of `text`, the text of the file `path`. Raises `ValueError` with each
/// problem as `<path>:<line>:<column>: <message>. <fix>`, by line.
fn decoded(path: &Path, text: &str) -> PyResult<config::profile::Profile> {
    config_hcl::read(Source(0), text)
        .and_then(|document| config::profile::decode(&document))
        .map_err(|problems| {
            let lines: Vec<String> = problems
                .iter()
                .map(|problem| {
                    let (line, column) = problem.span.map_or((0, 0), |span| {
                        (span.start().line, span.start().column)
                    });
                    format!("{}:{}:{}: {problem}", path.display(), line + 1, column + 1)
                })
                .collect();
            PyValueError::new_err(lines.join("\n"))
        })
}

/// The `ValueError` of the key file `path` that `error` refuses, as `<path>: <code>:
/// <message>. <fix>`. For a key with a passphrase, `Or <fix>` follows as a second
/// fix.
fn unreadable(path: &Path, error: Unreadable, fix: &str) -> PyErr {
    let (path, code, first) = (path.display(), error.code(), error.fix());
    let text = format!("{path}: {code}: {error}. {first}");
    PyValueError::new_err(match error {
        Unreadable::Passphrase => format!("{text}. Or {fix}"),
        Unreadable::Format
        | Unreadable::Algorithm(_)
        | Unreadable::Mismatch
        | Unreadable::Comment => text,
    })
}

/// The path `path` with `.<suffix>` after its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Makes `path` and each missing directory above it, each with mode 0700.
fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// The file that `path` names after each link, also a last link to no file.
/// [`fs::canonicalize`] refuses the last, so a caller tries it first: it also refuses
/// a loop of links.
fn linked(path: &Path) -> io::Result<PathBuf> {
    let mut path = path.to_owned();
    loop {
        match fs::symlink_metadata(&path) {
            Ok(file) if file.file_type().is_symlink() => {
                path = directory(&path).join(fs::read_link(&path)?);
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => return Ok(path),
        }
    }
}

/// The directory of the file `path`.
fn directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Syncs the entries of `directory` to the disk.
#[expect(
    clippy::disallowed_methods,
    reason = "the files of the program's own host"
)]
fn synced(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// Opens `path` with `options`, and with mode 0600 when it makes the file.
#[expect(
    clippy::disallowed_methods,
    reason = "the files of the program's own host"
)]
fn private_file(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options.open(path)
}
