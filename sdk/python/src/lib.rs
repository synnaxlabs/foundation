//! The native module `foundation._native` of the Python SDK: the Rust SDK `sdk`, for
//! Python.

use std::mem;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, PoisonError};

use pyo3::exceptions::{
    PyConnectionError, PyException, PyOSError, PyRuntimeError, PyTypeError,
    PyValueError,
};
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::types::{PyInt, PyString};
use types::authority::Authority;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;
use types::time::Span;

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
/// `node` is the `uuid.UUID` key of the node, `public_key` the 32 bytes of the key
/// that the node proves, and `private_key` the 32 bytes of a key that the spec lists
/// for `subject`. Each of `address`, the pair (`node`, `public_key`), and the pair
/// (`subject`, `private_key`) comes from the arguments, else from the node that runs on
/// the data directory `data`: that node gives its address and keys, and its first admin
/// gives the subject. `data` is a `str` or an `os.PathLike`, the `--data` of
/// `foundation start`, by default `foundation-data`. With an `address`, the data
/// directory counts only when its node is at that address: `127.0.0.1:p` matches a node
/// bound on `0.0.0.0:p`. Other Python threads run while it waits.
///
/// Raises `TypeError` for an argument of a wrong type, `ValueError` for an argument
/// that does not parse or a pair given in half, `RuntimeError` when a thread of the
/// client or of the read of `data` cannot start, `OSError` when the local UDP socket
/// cannot bind, `ConnectionError` when no source gives a value, the data directory
/// cannot be read, the client cannot reach the node, or the node refuses the subject,
/// and `PanicException` when a thread of the client or of the read of `data` panicked.
#[pyfunction]
#[pyo3(signature = (
    address=None, *, node=None, public_key=None, subject=None, private_key=None,
    data=None,
))]
fn connect(
    py: Python<'_>,
    address: Option<&str>,
    node: Option<&Bound<'_, PyAny>>,
    public_key: Option<&[u8]>,
    subject: Option<&str>,
    private_key: Option<&[u8]>,
    data: Option<PathBuf>,
) -> PyResult<Client> {
    let keys = pair(["node", "public_key"], node, public_key)?;
    let subject = pair(["subject", "private_key"], subject, private_key)?;
    let address = address.map(parse_address).transpose()?;
    let data = data.map(parse_data).transpose()?;
    let keys = keys.map(|(node, public_key)| node_keys(node, public_key));
    let subject = subject.map(|(name, key)| parse_subject(name, key));
    let given = Given {
        address,
        keys: keys.transpose()?,
        subject: subject.transpose()?,
    };
    let (address, node, subject) = match given {
        Given {
            address: Some(address),
            keys: Some((key, public_key)),
            subject: Some(subject),
        } => (address, (key, public_key), subject),
        given => {
            let data = data.unwrap_or_else(|| PathBuf::from("foundation-data"));
            given.sourced(&data, local(py, &data)?)?
        }
    };
    let (key, public_key) = node;
    let node = sdk::Node {
        addresses: vec![address],
        key,
        public_key,
    };
    let client = py
        .detach(|| sdk::Client::connect(&node, subject))
        .map_err(|e| error(py, &e))?;
    Ok(Client {
        session: Mutex::new(Some(client)),
        process: process::id(),
        address,
    })
}

/// The key and the public key of a node.
type Keys = (types::node::Key, PublicKey);

/// What the arguments of [`connect`] give.
struct Given {
    address: Option<SocketAddr>,
    keys: Option<Keys>,
    subject: Option<sdk::Subject>,
}

impl Given {
    /// The address, the node's keys, and the subject, each from the arguments, else
    /// from `local`, the node that runs on the data directory `data`, if any.
    fn sourced(
        self,
        data: &Path,
        local: Option<(sdk::Node, Option<sdk::Subject>)>,
    ) -> PyResult<(SocketAddr, Keys, sdk::Subject)> {
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
        let unsourced = |what: &str, pass: &str| {
            PyConnectionError::new_err(match other {
                Some(other) => format!(
                    "no source gives {what} at {address}: {} holds the node at \
                     {other}; pass {pass}, or connect to {other}",
                    data.display()
                ),
                None => format!(
                    "no source gives {what} at {address}: pass {pass}, or data= with \
                     the --data of foundation start on the node's host"
                ),
            })
        };
        let (node, admin) = local.unzip();
        let keys = self
            .keys
            .or_else(|| node.map(|node| (node.key, node.public_key)))
            .ok_or_else(|| {
                unsourced("the keys of the node", "node= and public_key=")
            })?;
        let subject = match (self.subject, admin) {
            (Some(subject), _) | (None, Some(Some(subject))) => subject,
            (None, Some(None)) => {
                return Err(PyConnectionError::new_err(format!(
                    "{} holds no admin.key that a node wrote",
                    data.display()
                )));
            }
            (None, None) => {
                return Err(unsourced(
                    "the subject for the node",
                    "subject= and private_key=",
                ));
            }
        };
        Ok((address, keys, subject))
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

/// The values of a pair of arguments `names`, or `None` when neither is given.
/// Raises `ValueError` when only one is.
fn pair<A, B>(
    names: [&str; 2],
    a: Option<A>,
    b: Option<B>,
) -> PyResult<Option<(A, B)>> {
    match (a, b) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        (None, None) => Ok(None),
        _ => Err(PyValueError::new_err(format!(
            "{} and {} go together: give both or neither",
            names[0], names[1]
        ))),
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

/// The node's keys of the arguments `node` and `public_key`.
fn node_keys(node: &Bound<'_, PyAny>, public_key: &[u8]) -> PyResult<Keys> {
    let public_key = PublicKey::new(bytes("public_key", public_key)?)
        .map_err(|e| PyValueError::new_err(format!("public_key: {e}")))?;
    Ok((key(node)?, public_key))
}

/// The subject of the arguments `subject` and `private_key`.
fn parse_subject(subject: &str, private_key: &[u8]) -> PyResult<sdk::Subject> {
    Ok(sdk::Subject {
        name: subject
            .parse::<Name>()
            .map_err(|e| PyValueError::new_err(format!("subject: {e}")))?,
        private_key: PrivateKey(bytes("private_key", private_key)?),
    })
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
    /// Raises `TypeError` for an argument of a wrong type; `ValueError` for no
    /// channel, a name that does not parse, an `authority` outside 0 to 255, a `lease`
    /// that is not a finite number over 0, or another `on_loss`, with no writer
    /// opened; `Stopped` when the node refused the open, with a text that names each
    /// channel of the open for `unknown`; `ConnectionError` when the session failed;
    /// `RuntimeError` when the client closed or in a process that the program forked;
    /// and `PanicException` when the client's thread panicked.
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
        lease: Option<f64>,
        on_loss: &str,
    ) -> PyResult<write::Writer> {
        let loss = write::Loss::parse(on_loss)?;
        let names = names(channels)?;
        let mut config = sdk::write::Config::new(names.clone());
        config.authority = authority.0;
        config.lease = lease.map(span).transpose()?;
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

/// The span of the lease `lease`, in seconds.
fn span(lease: f64) -> PyResult<Span> {
    let nanos = (lease * 1e9).round();
    // The bound is the first f64 past `i64::MAX`, and a NaN fails both tests.
    #[expect(clippy::cast_precision_loss, reason = "2^63 is exact in an f64")]
    if !(nanos >= 1.0 && nanos < i64::MAX as f64) {
        return Err(PyValueError::new_err(format!(
            "lease is {lease:?} s, not a finite number of seconds over 0"
        )));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the test above keeps a whole number of nanoseconds in i64"
    )]
    Ok(Span::from_nanos(nanos as i64))
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

/// The node key of the `uuid.UUID` `node`.
fn key(node: &Bound<'_, PyAny>) -> PyResult<types::node::Key> {
    let uuid = node.py().import("uuid")?.getattr("UUID")?;
    if !node.is_instance(&uuid)? {
        let kind = node.get_type().name()?;
        return Err(PyTypeError::new_err(format!(
            "node is {kind}, not uuid.UUID"
        )));
    }
    Ok(types::node::Key::from_u128(node.getattr("int")?.extract()?))
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
        sdk::Error::Session(_)
        | sdk::Error::Local(_)
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
