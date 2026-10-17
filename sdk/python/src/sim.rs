//! Test-only functions of the module, behind the feature `sim`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// Adds the functions of this module to `module`.
pub(crate) fn add(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(_panic, module)?)?;
    module.add_function(wrap_pyfunction!(_error, module)?)
}

/// Panics on the caller's thread. A build where panics unwind raises
/// `PanicException`; with `panic = "abort"`, the process ends.
#[pyfunction]
fn _panic() {
    panic!("the test panicked")
}

/// Raises the exception of the `sdk::Error` of `kind`: `thread` (with the reason
/// `"the test"`) or `panicked`. Raises `ValueError` for another kind.
#[pyfunction]
fn _error(kind: &str) -> PyResult<()> {
    let error = match kind {
        "thread" => sdk::Error::Thread {
            reason: "the test".into(),
        },
        "panicked" => sdk::Error::Panicked,
        kind => return Err(PyValueError::new_err(format!("no error {kind:?}"))),
    };
    Err(super::error(&error))
}
