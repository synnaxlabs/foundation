//! The native module `foundation._native` of the Python SDK: the Rust SDK `sdk`, for
//! Python.

use std::mem;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, PoisonError};

use config::openssh;
use pyo3::exceptions::{
    PyConnectionError, PyException, PyMemoryError, PyOSError, PyOverflowError,
    PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::types::{PyInt, PyString};
use types::authority::Authority;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;
use types::time::Span;

use crate::profile::Profile;

mod profile;
#[cfg(feature = "sim")]
mod sim;
#[cfg(test)]
mod tests;
mod write;

pyo3::create_exception!(
    foundation,
    Refused,
    PyConnectionError,
    "The node refused the session or closed it, with a code: connect again. `code` is \
     its name in lower case: `refused` when the spec does not list the subject or its \
     key, `unsynced` when the node has no mesh time yet, `stale`, `via`, `expired`, or \
     `changed` for a hello that the node refused, or a code of `Stopped`."
);
pyo3::create_exception!(
    foundation,
    Stopped,
    PyException,
    "The node stopped a write, with a code. At the open, no writer opened, and the \
     session goes on. A writer that raises it has ended, and each later call raises it \
     again. `code` is its name in lower case. At the open: `unknown` for a name of no \
     channel, `not_home` when the node is not the home of the index, `malformed` for \
     channels of more than one index or a lease that the home refuses, `failed` when \
     the home cannot open the write, `refused` when the access check of the node \
     refuses the write, and `busy` when the node has no room for the open. Later: \
     `unknown` when a channel was removed, `failed` when the home failed, `busy` when \
     the home has no room for the outcome of a frame, and `malformed` for a frame that \
     breaks a rule of the wire."
);
pyo3::create_exception!(
    foundation,
    Rejected,
    PyException,
    "The node refused one frame of a writer, and took none of it. `code` is why: \
     `waiting`, `reserved`, `expired`, `order`, `codec`, `full`, or `large`. The \
     writer goes on."
);
pyo3::create_exception!(
    foundation,
    DataLost,
    PyException,
    "The node found no room for a live frame, so its samples are a gap in the log. \
     `seqs` is a `dict` from the name of each channel of the frame to the `range` of \
     the seqs of its lost samples. The writer goes on, as a later frame can find room."
);

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add_function(wrap_pyfunction!(connect, module)?)?;
    module.add_class::<Client>()?;
    module.add_class::<write::Writer>()?;
    module.add("Refused", py.get_type::<Refused>())?;
    module.add("Stopped", py.get_type::<Stopped>())?;
    module.add("Rejected", py.get_type::<Rejected>())?;
    module.add("DataLost", py.get_type::<DataLost>())?;
    #[cfg(feature = "sim")]
    sim::add(module)?;
    Ok(())
}

/// Dials the node at `address` (`ip:port`, UDP), and blocks until it admits the first
/// hello of `subject`. The client then renews the hello until it closes.
///
/// `public_key` is the 32 bytes of the key that the node proves, and `private_key` the
/// 32 bytes of a key that the spec lists for `subject`. A `subject` with no
/// `private_key` takes the key of `~/.foundation/<subject>.key`. Each of `address`,
/// `public_key`, and `subject` comes from the first source that gives it: the
/// arguments, the profile, then the node that runs on the data directory `data`, which
/// gives its address and public key, and whose first admin gives the subject. `data` is
/// a `str` or an `os.PathLike`, the `--data` of `foundation start`, by default
/// `foundation-data`. With an `address`, the data directory counts only when its node
/// is at that address: `127.0.0.1:p` matches a node bound on `0.0.0.0:p`.
///
/// The profile is the HCL file that `FOUNDATION_PROFILE` names, else
/// `~/.foundation/profile.hcl`. Its `subject` gives the subject, with the key of its
/// `private_key` file, else of `~/.foundation/<subject>.key`, and each `node` block
/// pins the public key of the node at its address. With no other source, the subject
/// is the login name of the user, with the key of `~/.foundation/<login>.key`. A key
/// file that is not there is made, as `ssh-keygen -t ed25519 -N ""` makes it. With no
/// source of the public key, the node that answers at `address` is trusted on first
/// use: the profile pins its key, and the logger `foundation` gets one WARNING.
/// `strict`, else the profile's `strict`, refuses a node with no source of its key.
/// Other Python threads run while it waits.
///
/// Raises `TypeError` for an argument of a wrong type, `ValueError` for an argument
/// that does not parse, a `private_key` with no `subject`, a profile with a problem, or
/// a key file that is not the OpenSSH file of an Ed25519 key with no passphrase,
/// `RuntimeError` when a thread of the client or of the read of `data` cannot start,
/// `OSError` when the local UDP socket cannot bind or the profile or a key file cannot
/// be read or written, `MemoryError` when the OS refuses the memory of the pool of the
/// frames of the client's readers, `ConnectionError` when no source gives the address,
/// or with `strict` the public key, the data directory cannot be read, the client
/// cannot reach the node, the node does not prove the key that the profile pins, or the
/// node refuses the subject, and `PanicException` when a thread of the client or of the
/// read of `data` panicked.
#[pyfunction]
#[pyo3(signature = (
    address=None, *, public_key=None, subject=None, private_key=None, data=None,
    strict=None,
))]
fn connect(
    py: Python<'_>,
    address: Option<&str>,
    public_key: Option<&[u8]>,
    subject: Option<&str>,
    private_key: Option<&[u8]>,
    data: Option<PathBuf>,
    strict: Option<bool>,
) -> PyResult<Client> {
    let subject = parse_subject(subject, private_key)?;
    let address = address.map(parse_address).transpose()?;
    let data = data.map(parse_data).transpose()?;
    let public_key = public_key.map(parse_public_key).transpose()?;
    let given = Given {
        address,
        public_key,
        subject,
    };
    let (address, key, (subject, filed)) = match given {
        Given {
            address: Some(address),
            public_key: Some(public_key),
            subject: Some(Named::Keyed(subject)),
        } => (address, Key::Known(public_key), (subject, false)),
        given => {
            let data = data.unwrap_or_else(|| PathBuf::from("foundation-data"));
            given.sourced(py, &data, local(py, &data)?, strict)?
        }
    };
    let (public_key, profile) = match key {
        Key::Known(public_key) => (public_key, None),
        Key::Pinned(public_key, profile) => (public_key, Some(profile)),
        Key::Unknown(profile) => (trust(py, address, &profile)?, Some(profile)),
    };
    let pinned = profile.as_ref().map(|profile| (public_key, profile));
    let node = sdk::Node {
        addresses: vec![address],
        public_key,
    };
    let refused = filed.then(|| Refusal {
        subject: subject.name.clone(),
        public_key: subject.private_key.public(),
    });
    let client = py
        .detach(|| sdk::Client::connect(&node, subject))
        .map_err(|e| connect_error(py, &e, address, pinned, refused.as_ref()))?;
    Ok(Client {
        session: Mutex::new(Some(client)),
        process: process::id(),
        address,
    })
}

/// What the arguments of [`connect`] give.
struct Given {
    address: Option<SocketAddr>,
    public_key: Option<PublicKey>,
    subject: Option<Named>,
}

/// The subject that the arguments of [`connect`] give.
enum Named {
    /// `subject=` with `private_key=`.
    Keyed(sdk::Subject),
    /// `subject=` alone, with the key of `~/.foundation/<subject>.key`.
    Filed(Name),
}

/// Where the public key of the node comes from.
enum Key {
    /// An argument or the data directory.
    Known(PublicKey),
    /// The profile pins it.
    Pinned(PublicKey, Profile),
    /// No source: the node that answers is trusted on first use, and the profile
    /// pins it.
    Unknown(Profile),
}

impl Given {
    /// The address, the source of the node's public key, and the subject with whether
    /// its key comes from a key file. Each comes
    /// from the arguments, else from the profile, else from `local`, the node that
    /// runs on the data directory `data`, if any. Else the public key is unknown,
    /// unless `strict`, else the profile's `strict`. Else the subject is the login
    /// name of the user, with the key of `~/.foundation/<login>.key`.
    fn sourced(
        self,
        py: Python<'_>,
        data: &Path,
        local: Option<(sdk::Node, Option<sdk::Subject>)>,
        strict: Option<bool>,
    ) -> PyResult<(SocketAddr, Key, (sdk::Subject, bool))> {
        let (local, other) = match local {
            Some((node, _))
                if self
                    .address
                    .is_some_and(|address| !node.addresses.contains(&address)) =>
            {
                (None, node.addresses.first().copied())
            }
            local => (local, None),
        };
        let at = self.address.or_else(|| {
            let (node, _) = local.as_ref()?;
            node.addresses.first().copied()
        });
        let Some(address) = at else {
            return Err(PyConnectionError::new_err(format!(
                "no node runs in {}: pass the address of the node, or data= with the \
                 --data of foundation start",
                data.display()
            )));
        };
        let profile = Profile::read(py)?;
        let (node, admin) = local.unzip();
        let pinned = profile.held.nodes.get(&address).copied();
        let unknown = self.public_key.is_none() && pinned.is_none() && node.is_none();
        if unknown && strict.unwrap_or(profile.held.strict) {
            let held = other.map_or_else(String::new, |other| {
                format!("; {} holds the node at {other}", data.display())
            });
            return Err(PyConnectionError::new_err(format!(
                "no source gives the public key of the node at {address}, and `strict` \
                 refuses a node that no source pins: pass public_key=, or add the \
                 block that `foundation profile {address}` prints on the node's host \
                 to {}{held}",
                profile.path.display()
            )));
        }
        let subject = match (self.subject, &profile.held.subject, admin) {
            (Some(Named::Keyed(subject)), _, _) | (None, None, Some(Some(subject))) => {
                (subject, false)
            }
            (Some(Named::Filed(name)), _, _) => {
                let private_key = profile.private_key(py, &name, None)?;
                (sdk::Subject { name, private_key }, true)
            }
            (None, Some(name), _) => {
                let named = profile.held.private_key.as_deref();
                let private_key = profile.private_key(py, name, named)?;
                let name = name.clone();
                (sdk::Subject { name, private_key }, true)
            }
            (None, None, Some(None)) => {
                return Err(PyConnectionError::new_err(format!(
                    "{} holds no admin.key that a node wrote",
                    data.display()
                )));
            }
            (None, None, None) => (login(py, &profile)?, true),
        };
        let key = match (self.public_key, pinned, node) {
            (Some(public_key), _, _)
            | (None, None, Some(sdk::Node { public_key, .. })) => {
                Key::Known(public_key)
            }
            (None, Some(public_key), _) => Key::Pinned(public_key, profile),
            (None, None, None) => Key::Unknown(profile),
        };
        Ok((address, key, subject))
    }
}

/// The subject of the login name of the user, with the key of
/// `~/.foundation/<login>.key`. Raises `ValueError` with the fix in `profile` when
/// the login name is not a subject name.
fn login(py: Python<'_>, profile: &Profile) -> PyResult<sdk::Subject> {
    let login: String = py.import("getpass")?.call_method0("getuser")?.extract()?;
    let name = login.parse::<Name>().map_err(|e| {
        PyValueError::new_err(format!(
            "the login name {login:?} is not a subject name: {e}; add `subject = \
             \"<name>\"` to {}, or pass subject=",
            profile.path.display()
        ))
    })?;
    Ok(sdk::Subject {
        private_key: profile.private_key(py, &name, None)?,
        name,
    })
}

/// The public key that the node at `address` proves, which `profile` then pins, or the
/// key that a program pinned in `profile` meanwhile. Logs one WARNING on the logger
/// `foundation`. Other Python threads run while it dials.
fn trust(
    py: Python<'_>,
    address: SocketAddr,
    profile: &Profile,
) -> PyResult<PublicKey> {
    let node = py
        .detach(|| sdk::Node::answering(vec![address]))
        .map_err(|e| error(py, &e))?;
    let public_key = profile.pin(address, node.public_key)?;
    py.import("logging")?
        .call_method1("getLogger", ("foundation",))?
        .call_method1(
            "warning",
            (format!(
                "trusted the node at {address} on first use: public key {}; it is \
                 pinned in {}",
                openssh::fingerprint(public_key),
                profile.path.display()
            ),),
        )?;
    Ok(public_key)
}

/// The subject that a hello names, for the text of a refusal.
struct Refusal {
    subject: Name,
    public_key: PublicKey,
}

/// The Python exception of `error`, the failed connect to the node at `address`. When
/// `pinned` is the key that a profile pins and the node proves another, it names both.
/// When the node refuses `refused`, the subject of a key file, it gives the blocks of
/// the spec that admit it.
fn connect_error(
    py: Python<'_>,
    error: &sdk::Error,
    address: SocketAddr,
    pinned: Option<(PublicKey, &Profile)>,
    refused: Option<&Refusal>,
) -> PyErr {
    match (error, pinned) {
        (
            sdk::Error::Session(program::Error::Transport(_)),
            Some((pinned, profile)),
        ) => match py.detach(|| sdk::Node::answering(vec![address])) {
            Ok(node) if node.public_key != pinned => {
                PyConnectionError::new_err(format!(
                    "the node at {address} proves public key {}, but {} pins {}: \
                     when the node was made again, remove its `node \"{address}\"` \
                     block from {}",
                    openssh::fingerprint(node.public_key),
                    profile.path.display(),
                    openssh::fingerprint(pinned),
                    profile.path.display(),
                ))
            }
            _ => self::error(py, error),
        },
        (
            sdk::Error::Session(program::Error::Refused(program::Refusal::Refused)),
            _,
        ) if let Some(Refusal {
            subject,
            public_key,
        }) = refused =>
        {
            let text = format!(
                "{error}. If the spec of the node does not list subject {subject} with \
                 this key, add to its config files on the node's host:\n\n\
                 subject \"{subject}\" {{\n  keys = \"{}\"\n}}\n\n\
                 access \"{subject}\" {{\n  subjects = \"{subject}\"\n  select   = \
                 \"**\"\n  allow    = [\"read\"]\n}}\n\n\
                 then run `foundation plan <files> --out <plan>` and `foundation \
                 apply <plan>` there",
                openssh::line(*public_key),
            );
            exception(py, error, text)
        }
        _ => self::error(py, error),
    }
}

/// The node that runs on the data directory `data` and its first admin, or `None`
/// when no node runs on it. Other Python threads run while it reads.
fn local(
    py: Python<'_>,
    data: &Path,
) -> PyResult<Option<(sdk::Node, Option<sdk::Subject>)>> {
    match py.detach(|| sdk::Node::local(data)) {
        Ok(local) => Ok(Some(local)),
        Err(sdk::Error::Local(sector::local::Error::NotRunning)) => Ok(None),
        Err(sdk::Error::Local(sector::local::Error::Version {
            file,
            found,
            expected,
        })) => Err(PyConnectionError::new_err(format!(
            "{} holds format version {}, and this package reads version {}: install \
             the package of the node's version",
            data.join("data").join(file).display(),
            char::from(found).escape_default(),
            char::from(expected).escape_default(),
        ))),
        Err(sdk::Error::Local(sector::local::Error::Files(e))) => {
            Err(PyConnectionError::new_err(format!(
                "cannot read {}: {e}",
                data.join("data").display()
            )))
        }
        Err(e) => Err(error(py, &e)),
    }
}

/// The socket address of `address`.
fn parse_address(address: &str) -> PyResult<SocketAddr> {
    address.parse().map_err(|e| {
        PyValueError::new_err(format!("the address {address:?} is not ip:port: {e}"))
    })
}

/// The data directory of the argument `data`, which names a file only with no NUL byte.
fn parse_data(data: PathBuf) -> PyResult<PathBuf> {
    if data.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(PyValueError::new_err("data: the path holds a NUL byte"));
    }
    Ok(data)
}

/// The node's public key of the argument `public_key`.
fn parse_public_key(public_key: &[u8]) -> PyResult<PublicKey> {
    PublicKey::new(bytes("public_key", public_key)?)
        .map_err(|e| PyValueError::new_err(format!("public_key: {e}")))
}

/// The subject of the arguments `subject` and `private_key`, or `None` when neither
/// is given. Raises `ValueError` for `private_key` with no `subject`.
fn parse_subject(
    subject: Option<&str>,
    private_key: Option<&[u8]>,
) -> PyResult<Option<Named>> {
    let Some(subject) = subject else {
        return match private_key {
            Some(_) => Err(PyValueError::new_err(
                "private_key is the key of a subject: pass subject= too",
            )),
            None => Ok(None),
        };
    };
    let name = subject
        .parse::<Name>()
        .map_err(|e| PyValueError::new_err(format!("subject: {e}")))?;
    Ok(Some(match private_key {
        Some(private_key) => Named::Keyed(sdk::Subject {
            name,
            private_key: PrivateKey(bytes("private_key", private_key)?),
        }),
        None => Named::Filed(name),
    }))
}

/// A program's session with one node. A `with` block closes it at its end. A client
/// that the program drops without a close ends its session and does not wait. A
/// process that forks from the program does not own the client: in it, a close gives
/// `None`, and neither a close nor a drop ends the session.
#[pyclass(frozen, module = "foundation")]
struct Client {
    session: Mutex<Option<sdk::Client>>,
    /// The process that connected. A child that it forks has no thread of the client,
    /// and can copy the lock as held.
    process: u32,
    /// The address that `connect` dialed.
    address: SocketAddr,
}

#[pymethods]
impl Client {
    /// The address that `connect` dialed, as `ip:port`.
    #[getter]
    fn address(&self) -> String {
        self.address.to_string()
    }

    /// Closes the session, and blocks until the close goes out, the transport drains
    /// each close (up to 3 s), and the client's thread ends. A close during another
    /// close waits for it to end, and gives `None`, as a close after it does. A close
    /// during an `open_writer` of another thread also waits for that open to end.
    ///
    /// Raises `ConnectionError` with the cause when the session ended before the close,
    /// and `PanicException` when the client's thread panicked. Only the close that ends
    /// the session raises.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if process::id() != self.process {
            return Ok(());
        }
        // The lock is taken without the GIL, which the closing thread needs to return.
        py.detach(|| {
            let mut client =
                self.session.lock().unwrap_or_else(PoisonError::into_inner);
            client.take().map_or(Ok(()), sdk::Client::close)
        })
        .map_err(|e| error(py, &e))
    }

    /// Opens a writer of `channels` at the home of their index, and blocks until the
    /// node opened it. `channels` is one channel name or an iterable of names, all of
    /// one index. Place `n` is `channels[n]`, and the index is place `len(channels)`
    /// when `channels` does not name it. `authority` (0 to 255) is the writer's
    /// authority over its channels: a writer of a higher authority takes control from
    /// it. `lease` is the seconds that the writer may go with no write and keep
    /// control, or `None` for no limit. `on_loss` is what a write does when the node
    /// took no room for a live frame: `"warn"` logs a warning on the `foundation`
    /// logger, `"raise"` raises `DataLost`, and `"ignore"` does nothing. Other Python
    /// threads run while it waits.
    ///
    /// Raises `TypeError` for an argument of a wrong type; `ValueError` for no channel,
    /// a name that does not parse, an `authority` outside 0 to 255, a `lease` that,
    /// taken as a `float`, does not round to 1 to 2^63 - 1 nanoseconds, or another
    /// `on_loss`, with no writer opened; `Stopped` when the node refused the open, with
    /// a text that names each channel of the open for `unknown`; `ConnectionError` when
    /// the session failed; `RuntimeError` when the client closed or in a process that
    /// the program forked; and `PanicException` when the client's thread panicked.
    #[pyo3(
        signature = (
            channels, *, authority=Level(Authority(0)), lease=None, on_loss="warn"
        ),
        text_signature = "(self, channels, *, authority=0, lease=None, on_loss='warn')"
    )]
    fn open_writer(
        &self,
        py: Python<'_>,
        channels: &Bound<'_, PyAny>,
        authority: Level,
        lease: Option<Lease>,
        on_loss: &str,
    ) -> PyResult<write::Writer> {
        let loss = write::Loss::parse(on_loss)?;
        let names = names(channels)?;
        let mut config = sdk::write::Config::new(names.clone());
        config.authority = authority.0;
        config.lease = lease.map(|lease| lease.0);
        owned(self.process)?;
        // The lock is taken without the GIL, which a closing thread needs to return.
        let writer = py
            .detach(|| {
                let client =
                    self.session.lock().unwrap_or_else(PoisonError::into_inner);
                client
                    .as_ref()
                    .ok_or(sdk::Error::Closed)?
                    .open_writer(config)
            })
            .map_err(|e| {
                if !matches!(
                    e,
                    sdk::Error::Stream(program::Error::Refused(
                        program::Refusal::Unknown
                    ))
                ) {
                    return error(py, &e);
                }
                let names: Vec<_> = names.iter().map(Name::as_str).collect();
                exception(py, &e, format!("{e}: the open names {}", names.join(", ")))
            })?;
        Ok(write::Writer::new(writer, self.process, loss))
    }

    fn __enter__(this: Py<Self>) -> Py<Self> {
        this
    }

    /// Closes the client. When the block raised, an error of the close is a note on
    /// the error of the block; else it raises.
    #[pyo3(signature = (_kind, raised, _traceback))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _kind: &Bound<'_, PyAny>,
        raised: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        exit(self.close(py), raised)
    }
}

/// The outcome of a `__exit__` whose close gave `closed`: when the block raised
/// `raised`, an error of the close is a note on it.
fn exit(closed: PyResult<()>, raised: &Bound<'_, PyAny>) -> PyResult<()> {
    match closed {
        Err(close) if !raised.is_none() => {
            raised.call_method1("add_note", (format!("the close raised {close}"),))?;
            Ok(())
        }
        closed => closed,
    }
}

/// Raises `RuntimeError` in a process other than `process`, the process that
/// connected.
fn owned(process: u32) -> PyResult<()> {
    let now = process::id();
    if now == process {
        return Ok(());
    }
    Err(PyRuntimeError::new_err(format!(
        "the client connected in process {process}, and this is process {now}: \
         connect again in this process"
    )))
}

/// The channel names of the argument `channels`: a name or an iterable of names.
fn names(channels: &Bound<'_, PyAny>) -> PyResult<Vec<Name>> {
    let parse = |name: &Bound<'_, PyAny>| -> PyResult<Name> {
        let name = name.cast::<PyString>().map_err(|_other| {
            let kind = name.get_type().name();
            PyTypeError::new_err(match kind {
                Ok(kind) => format!("a channel name is a str, not {kind}"),
                Err(_) => "a channel name is a str".to_owned(),
            })
        })?;
        name.to_str()?
            .parse::<Name>()
            .map_err(|e| PyValueError::new_err(format!("channels: {e}")))
    };
    if channels.is_instance_of::<PyString>() {
        return Ok(vec![parse(channels)?]);
    }
    channels.try_iter()?.map(|name| parse(&name?)).collect()
}

/// The argument `authority` of `open_writer`: an `int` from 0 to 255.
#[derive(Clone, Copy)]
struct Level(Authority);

impl<'a, 'py> FromPyObject<'a, 'py> for Level {
    type Error = PyErr;

    fn extract(object: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let int = object.cast::<PyInt>().map_err(|_other| {
            match object.get_type().name() {
                Ok(kind) => {
                    PyTypeError::new_err(format!("authority is an int, not {kind}"))
                }
                Err(e) => e,
            }
        })?;
        let level = int.extract().map_err(|_outside| {
            PyValueError::new_err(format!("authority is {}, outside 0 to 255", *int))
        })?;
        Ok(Self(Authority(level)))
    }
}

/// The argument `lease` of `open_writer`: seconds that, taken as an `f64`, round to 1
/// to 2^63 - 1 nanoseconds.
struct Lease(Span);

impl<'a, 'py> FromPyObject<'a, 'py> for Lease {
    type Error = PyErr;

    fn extract(object: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        let refused = |lease: &str| {
            PyValueError::new_err(format!(
                "lease is {lease}, which does not round to 1 to 2^63 - 1 ns"
            ))
        };
        let lease = match object.extract::<f64>() {
            Ok(lease) => lease,
            Err(e) if e.is_instance_of::<PyOverflowError>(object.py()) => {
                let lease = object.repr().map_or_else(
                    |_| "past the range of a float".to_owned(),
                    |repr| format!("{repr} s"),
                );
                return Err(refused(&lease));
            }
            Err(e) => return Err(e),
        };
        let nanos = (lease * 1e9).round();
        // The bound is the first f64 past `i64::MAX`, and a NaN fails both tests.
        #[expect(clippy::cast_precision_loss, reason = "2^63 is exact in an f64")]
        if !(nanos >= 1.0 && nanos < i64::MAX as f64) {
            return Err(refused(&format!("{lease:?} s")));
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the test above keeps a whole number of nanoseconds in i64"
        )]
        Ok(Self(Span::from_nanos(nanos as i64)))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if process::id() != self.process {
            // `get_mut` takes no lock, so a lock that the fork copied as held does not
            // block.
            let session = self
                .session
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner);
            #[expect(
                clippy::mem_forget,
                reason = "the drop of the session wakes the parent's runtime and \
                          detaches its thread, which this process does not have"
            )]
            mem::forget(session.take());
        }
    }
}

/// The 32 bytes of the argument `name`.
fn bytes(name: &str, value: &[u8]) -> PyResult<[u8; 32]> {
    value.try_into().map_err(|_short| {
        PyValueError::new_err(format!("{name} is {} bytes, not 32", value.len()))
    })
}

/// The Python exception of `error`.
fn error(py: Python<'_>, error: &sdk::Error) -> PyErr {
    exception(py, error, error.to_string())
}

/// The Python exception of `error`, with the text `text`.
fn exception(py: Python<'_>, error: &sdk::Error, text: String) -> PyErr {
    match error {
        sdk::Error::Session(program::Error::Refused(refusal)) => {
            coded(py, Refused::new_err(text), *refusal)
        }
        sdk::Error::Stream(program::Error::Refused(refusal)) => {
            coded(py, Stopped::new_err(text), *refusal)
        }
        sdk::Error::Stream(
            program::Error::Layout(_)
            | program::Error::Unsent { .. }
            | program::Error::Pool(_)
            | program::Error::Body { .. }
            | program::Error::Open { .. }
            | program::Error::Unnamed,
        ) => PyValueError::new_err(text),
        sdk::Error::Thread { .. } | sdk::Error::Closed => PyRuntimeError::new_err(text),
        sdk::Error::Bind(_) => PyOSError::new_err(text),
        sdk::Error::Memory(_) => PyMemoryError::new_err(text),
        sdk::Error::Session(_)
        | sdk::Error::Local(_)
        | sdk::Error::Decode { .. }
        | sdk::Error::Stream(
            program::Error::Transport(_)
            | program::Error::Message(_)
            | program::Error::Unanswered
            | program::Error::Reply(_)
            | program::Error::Frame(_)
            | program::Error::Unplaced(_),
        ) => PyConnectionError::new_err(text),
        sdk::Error::Panicked => PanicException::new_err(text),
        sdk::Error::Value { .. } => PyTypeError::new_err(text),
    }
}

/// `error` with the attribute `code`, the code of `refusal`.
fn coded(py: Python<'_>, error: PyErr, refusal: program::Refusal) -> PyErr {
    match error.value(py).setattr("code", code(refusal)) {
        Ok(()) => error,
        Err(e) => e,
    }
}

/// The code of `refusal` in lower case.
fn code(refusal: program::Refusal) -> &'static str {
    match refusal {
        program::Refusal::Malformed => "malformed",
        program::Refusal::Unknown => "unknown",
        program::Refusal::NotHome => "not_home",
        program::Refusal::Failed => "failed",
        program::Refusal::Busy => "busy",
        program::Refusal::Refused => "refused",
        program::Refusal::Unsynced => "unsynced",
        program::Refusal::Stale => "stale",
        program::Refusal::Via => "via",
        program::Refusal::Expired => "expired",
        program::Refusal::Changed => "changed",
    }
}
