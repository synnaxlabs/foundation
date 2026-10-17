//! The native module `foundation._native` of the Python SDK: the Rust SDK `sdk`, for
//! Python.

use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

use pyo3::exceptions::{
    PyConnectionError, PyOSError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;

#[cfg(feature = "sim")]
mod sim;

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(connect, module)?)?;
    module.add_class::<Client>()?;
    #[cfg(feature = "sim")]
    sim::add(module)?;
    Ok(())
}

/// Dials the node at `address` (`ip:port`, UDP), and blocks until it admits the first
/// hello of `subject`. The client then renews the hello until it closes.
///
/// `node` is the `uuid.UUID` key of the node, `public_key` the 32 bytes of the key
/// that the node proves, and `private_key` the 32 bytes of a key that the spec lists
/// for `subject`. Other Python threads run while it waits.
///
/// Raises `TypeError` for an argument of a wrong type, `ValueError` for an argument
/// that does not parse, `RuntimeError` when the client's thread cannot start,
/// `OSError` when the local UDP socket cannot bind, `ConnectionError` when the client
/// cannot reach the node or the node refuses the subject, and `PanicException` when
/// the client's thread panicked.
#[pyfunction]
#[pyo3(signature = (address, *, node, public_key, subject, private_key))]
fn connect(
    py: Python<'_>,
    address: &str,
    node: &Bound<'_, PyAny>,
    public_key: &[u8],
    subject: &str,
    private_key: &[u8],
) -> PyResult<Client> {
    let address: SocketAddr = address.parse().map_err(|e| {
        PyValueError::new_err(format!("the address {address:?} is not ip:port: {e}"))
    })?;
    let public_key = PublicKey::new(bytes("public_key", public_key)?)
        .map_err(|e| PyValueError::new_err(format!("public_key: {e}")))?;
    let node = sdk::Node {
        addresses: vec![address],
        key: key(node)?,
        public_key,
    };
    let subject = sdk::Subject {
        name: subject
            .parse::<Name>()
            .map_err(|e| PyValueError::new_err(format!("subject: {e}")))?,
        private_key: PrivateKey(bytes("private_key", private_key)?),
    };
    let client = py
        .detach(|| sdk::Client::connect(&node, subject))
        .map_err(|e| error(&e))?;
    Ok(Client(Mutex::new(Some(client))))
}

/// A program's session with one node. A `with` block closes it at its end. A client
/// that the program drops without a close ends its session and does not wait.
#[pyclass(frozen, module = "foundation")]
struct Client(Mutex<Option<sdk::Client>>);

#[pymethods]
impl Client {
    /// Closes the session, and blocks until the close goes out and the client's thread
    /// ends. A second close does nothing.
    ///
    /// Raises `ConnectionError` with the cause when the session ended before the close,
    /// and `PanicException` when the client's thread panicked.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        // The guard drops before `detach`: a second caller waits for the lock with the
        // GIL, which the closing thread needs to return.
        let client = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        match client {
            Some(client) => py.detach(|| client.close()).map_err(|e| error(&e)),
            None => Ok(()),
        }
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
        match self.close(py) {
            Err(close) if !raised.is_none() => {
                raised
                    .call_method1("add_note", (format!("the close raised {close}"),))?;
                Ok(())
            }
            closed => closed,
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
fn error(error: &sdk::Error) -> PyErr {
    let text = error.to_string();
    match error {
        sdk::Error::Thread { .. } => PyRuntimeError::new_err(text),
        sdk::Error::Bind(_) => PyOSError::new_err(text),
        sdk::Error::Session(_) => PyConnectionError::new_err(text),
        sdk::Error::Panicked => PanicException::new_err(text),
    }
}
