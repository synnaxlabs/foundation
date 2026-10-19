//! Test-only functions of the module, behind the feature `sim`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
fn _error(py: Python<'_>, kind: &str) -> PyResult<()> {
    let error = match kind {
        "thread" => sdk::Error::Thread {
            reason: "the test".into(),
        },
        "panicked" => sdk::Error::Panicked,
        kind => return Err(PyValueError::new_err(format!("no error {kind:?}"))),
    };
    Err(super::error(py, &error))
}

/// The time from the start of the last call of a writer that reached its `sdk` call to
/// just before that call, which the bench reads.
#[derive(Default)]
pub(crate) struct Part(AtomicU64);

impl Part {
    /// The start of a call.
    pub(crate) fn start() -> Instant {
        #[expect(clippy::disallowed_methods, reason = "the bench times a call")]
        Instant::now()
    }

    /// Keeps the time since `began`, the start of a call.
    pub(crate) fn mark(&self, began: Instant) {
        #[expect(clippy::disallowed_methods, reason = "the bench times a call")]
        let nanos = began.elapsed().as_nanos();
        self.0
            .store(u64::try_from(nanos).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// The kept time, in nanoseconds.
    pub(crate) fn nanos(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
