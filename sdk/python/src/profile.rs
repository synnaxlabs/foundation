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

/// The profile of the host, and the file that holds it.
pub(crate) struct Profile {
    /// The file: the one that `FOUNDATION_PROFILE` names, else
    /// `~/.foundation/profile.hcl`. An empty `FOUNDATION_PROFILE` counts as unset.
    pub(crate) path: PathBuf,
    /// What the file holds, or an empty profile when it is not there or not read.
    pub(crate) held: config::profile::Profile,
}

impl Profile {
    /// The profile of the host, with nothing of its file read.
    pub(crate) fn unread(py: Python<'_>) -> PyResult<Self> {
        let environ = py.import("os")?.getattr("environ")?;
        let named: Option<PathBuf> = environ
            .call_method1("get", ("FOUNDATION_PROFILE",))?
            .extract()?;
        let path = match named.filter(|path| !path.as_os_str().is_empty()) {
            Some(path) => path,
            None => home(py)?.join("profile.hcl"),
        };
        let held = config::profile::Profile::default();
        Ok(Self { path, held })
    }

    /// Reads the profile of the host. Raises `ValueError` for a profile with a problem,
    /// with each problem by line, or that is not UTF-8, and `OSError` when the file
    /// cannot be read.
    pub(crate) fn read(py: Python<'_>) -> PyResult<Self> {
        let mut profile = Self::unread(py)?;
        let text = text(&profile.path)?.unwrap_or_default();
        profile.held = decoded(&profile.path, &text)?;
        Ok(profile)
    }

    /// Pins `public_key` as the key of the node at `address` in the file, unless a
    /// `node` block of `address` is there, and returns the key that the file pins.
    /// Each other byte of the file stays, and a file that is a link stays one. Two
    /// programs that pin at once take turns under a lock of the file `<path>.lock`,
    /// and the file changes in one rename, which is on the disk before it returns.
    /// Makes each missing directory of the path with mode 0700, and the file with
    /// mode 0600. Raises `ValueError` when the file has a problem, and `OSError` when
    /// it cannot be read or written.
    pub(crate) fn pin(
        &self,
        address: SocketAddr,
        public_key: PublicKey,
    ) -> PyResult<PublicKey> {
        let os =
            |e: io::Error| PyOSError::new_err(format!("{}: {e}", self.path.display()));
        private_directory(directory(&self.path)).map_err(os)?;
        let path = match fs::canonicalize(&self.path) {
            Ok(path) => path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.path.clone(),
            Err(e) => return Err(os(e)),
        };
        let lock = private_file(
            OpenOptions::new().write(true).create(true),
            &sibling(&path, "lock"),
        )
        .map_err(os)?;
        lock.lock().map_err(os)?;
        let text = text(&path)?.unwrap_or_default();
        if let Some(pinned) = decoded(&self.path, &text)?.nodes.get(&address) {
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
    /// `named` path that starts with `~` is expanded as the shell does, and a relative
    /// one is in the directory of the profile. Raises `ValueError` as [`made`] does,
    /// and `OSError` when a file cannot be read or written, or `named` is not there.
    pub(crate) fn private_key(
        &self,
        py: Python<'_>,
        subject: &Name,
        named: Option<&Path>,
    ) -> PyResult<PrivateKey> {
        let default = home(py)?.join(format!("{subject}.key"));
        let Some(named) = named else {
            let fix = "move the file away, and connect makes a new key there";
            return made(subject, &default, fix);
        };
        let fix = format!(
            "remove `private_key` from {}, and connect makes {}",
            self.path.display(),
            default.display()
        );
        let path = self.path.with_file_name("").join(expanded(py, named)?);
        key(&path, &fix)?.ok_or_else(|| {
            PyOSError::new_err(format!("{}: no file is there: {fix}", path.display()))
        })
    }
}

/// The directory `~/.foundation`, where the profile and each key file of a subject
/// are by default.
fn home(py: Python<'_>) -> PyResult<PathBuf> {
    expanded(py, Path::new("~/.foundation"))
}

/// `path` with a leading `~` or `~user` expanded to the home directory.
fn expanded(py: Python<'_>, path: &Path) -> PyResult<PathBuf> {
    py.import("os.path")?
        .call_method1("expanduser", (path,))?
        .extract()
}

/// The private key of `subject` in the OpenSSH file `path`. When there is no file,
/// makes a new key and writes it there with mode 0600, as `ssh-keygen -t ed25519 -N
/// ""` writes it, and makes each missing directory with mode 0700. The file is on the
/// disk before it returns. Two programs that make it at once get one key. Raises
/// `ValueError` when the file is not UTF-8 or not the key file of an Ed25519 key with
/// no passphrase, with `fix` for a key with a passphrase, and `OSError` when it
/// cannot be read or written.
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
        Err(e) => key(path, fix)?.ok_or_else(|| {
            PyOSError::new_err(format!("{}: {e}, and it names no file", path.display()))
        }),
    }
}

/// The private key in the OpenSSH file `path`, or `None` when there is no file.
/// Raises as [`text`] does, and `ValueError` when it is not the key file of an Ed25519
/// key with no passphrase, with `fix` for a key with a passphrase.
fn key(path: &Path, fix: &str) -> PyResult<Option<PrivateKey>> {
    text(path)?
        .map(|text| openssh::private_key(&text).map_err(|e| unreadable(path, e, fix)))
        .transpose()
}

/// The text of the file `path`, or `None` when there is no file. Raises `ValueError`
/// when it is not UTF-8, and `OSError` when it cannot be read.
fn text(path: &Path) -> PyResult<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(PyValueError::new_err(
            format!("{}: the file is not UTF-8", path.display()),
        )),
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
