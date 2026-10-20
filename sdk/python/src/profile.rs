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
    /// `~/.foundation/profile.hcl`.
    pub(crate) path: PathBuf,
    /// What the file holds, or an empty profile when it is not there.
    pub(crate) held: config::profile::Profile,
}

impl Profile {
    /// Reads the profile of the host. Raises `ValueError` for a profile with a
    /// problem, with each problem by line, and `OSError` when the file cannot be read.
    pub(crate) fn read(py: Python<'_>) -> PyResult<Self> {
        let environ = py.import("os")?.getattr("environ")?;
        let named: Option<PathBuf> = environ
            .call_method1("get", ("FOUNDATION_PROFILE",))?
            .extract()?;
        let path = match named {
            Some(path) => path,
            None => home(py)?.join("profile.hcl"),
        };
        let text = text(&path)?;
        let held = decoded(&path, &text)?;
        Ok(Self { path, held })
    }

    /// Pins `public_key` as the key of the node at `address` in the file, unless a
    /// `node` block of `address` is there, and returns the key that the file pins.
    /// Each other byte of the file stays. Two programs that pin at once take turns
    /// under a lock of the file `<path>.lock`, and the file changes in one rename.
    /// Makes each missing directory of the path with mode 0700. Raises `ValueError`
    /// when the file has a problem, and `OSError` when it cannot be read or written.
    pub(crate) fn pin(
        &self,
        address: SocketAddr,
        public_key: PublicKey,
    ) -> PyResult<PublicKey> {
        let os =
            |e: io::Error| PyOSError::new_err(format!("{}: {e}", self.path.display()));
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            private_directory(parent).map_err(os)?;
        }
        let lock_path = sibling(&self.path, "lock");
        let lock =
            private_file(OpenOptions::new().write(true).create(true), &lock_path)
                .map_err(os)?;
        lock.lock().map_err(os)?;
        let text = text(&self.path)?;
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
        let temporary = sibling(&self.path, "new");
        fs::write(&temporary, text).map_err(os)?;
        fs::rename(&temporary, &self.path).map_err(os)?;
        Ok(public_key)
    }

    /// The private key of `subject` in the key file `named`, relative to the directory
    /// of the profile, else in `~/.foundation/<subject>.key`. Raises as [`key_file`]
    /// does.
    pub(crate) fn private_key(
        &self,
        py: Python<'_>,
        subject: &Name,
        named: Option<&Path>,
    ) -> PyResult<PrivateKey> {
        let default = home(py)?.join(format!("{subject}.key"));
        match named {
            Some(named) => {
                let fix = format!(
                    "remove `private_key` from {}, and connect makes {}",
                    self.path.display(),
                    default.display()
                );
                key_file(subject, &self.path.with_file_name("").join(named), &fix)
            }
            None => key_file(
                subject,
                &default,
                "move the file away, and connect makes a new key there",
            ),
        }
    }
}

/// The directory `~/.foundation`, where the profile and each key file of a subject
/// are by default.
fn home(py: Python<'_>) -> PyResult<PathBuf> {
    py.import("os.path")?
        .call_method1("expanduser", ("~/.foundation",))?
        .extract()
}

/// The private key of `subject` in the OpenSSH file `path`. When there is no file,
/// makes a new key and writes it there with mode 0600, as `ssh-keygen -t ed25519 -N
/// ""` writes it, and makes each missing directory with mode 0700. Two programs that
/// make it at once get one key. Raises `ValueError` when the file is not the key file
/// of an Ed25519 key with no passphrase, with `fix` for a key with a passphrase, and
/// `OSError` when it cannot be read or written.
fn key_file(subject: &Name, path: &Path, fix: &str) -> PyResult<PrivateKey> {
    let os = |e: io::Error| PyOSError::new_err(format!("{}: {e}", path.display()));
    match fs::read_to_string(path) {
        Ok(text) => openssh::private_key(&text).map_err(|e| unreadable(path, e, fix)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = sdk::private_key();
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                private_directory(parent).map_err(os)?;
            }
            let temporary = sibling(path, &format!("{}.new", process::id()));
            let mut file = private_file(
                OpenOptions::new().write(true).create_new(true),
                &temporary,
            )
            .map_err(os)?;
            let linked = file
                .write_all(openssh::key_file(&key, subject.as_str()).as_bytes())
                .and_then(|()| fs::hard_link(&temporary, path));
            let removed = fs::remove_file(&temporary);
            match linked.and(removed) {
                Ok(()) => Ok(key),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    key_file(subject, path, fix)
                }
                Err(e) => Err(os(e)),
            }
        }
        Err(e) => Err(os(e)),
    }
}

/// The text of the file `path`, or an empty text when it is not there.
fn text(path: &Path) -> PyResult<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
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

/// The `ValueError` of the key file `path` that `error` refuses, with `fix` for a key
/// with a passphrase.
fn unreadable(path: &Path, error: Unreadable, fix: &str) -> PyErr {
    let path = path.display();
    PyValueError::new_err(match error {
        Unreadable::Passphrase => format!("{path}: {error}: {fix}"),
        error => format!("{path}: {error}"),
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
