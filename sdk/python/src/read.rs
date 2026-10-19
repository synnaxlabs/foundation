//! `foundation.Reader` and `foundation.Frame`: a reader of `sdk`, for Python.

use std::mem;
use std::ops::Range;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use numpy::PyArray1;
use numpy::datetime::{Datetime, Timedelta, units::Nanoseconds};
use pyo3::exceptions::{PyConnectionError, PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyDict, PyString, PyTuple};
use sdk::Place;
use sdk::read::Mode;
use types::sample::{Scalar, Type};

use crate::{Loss, owned};

/// The longest wait of `__next__` between two runs of the signal handlers.
const TICK: Duration = Duration::from_millis(100);

/// The mode of the argument `mode`.
pub(crate) fn mode(mode: &str) -> PyResult<Mode> {
    match mode {
        "complete" => Ok(Mode::Complete),
        "latest" => Ok(Mode::Latest),
        other => Err(PyValueError::new_err(format!(
            "mode is {other:?}, not \"complete\" or \"latest\""
        ))),
    }
}

/// A reader of channels of one index, which `Client.open_reader` opens. `for frame in
/// reader:` gives each frame as it comes, and ends after a close. A `with` block closes
/// it at its end. Read each frame as it comes: a frame that the node sent and the
/// program did not read holds a part of the receive window of the client, and when the
/// window is full, each writer and reader of the client waits. Read on a client of its
/// own when the program cannot. A process that forks from the program does not own the
/// reader: in it, each call but a close raises `RuntimeError`, and a close gives `None`
/// and ends nothing.
#[pyclass(frozen, module = "foundation")]
pub(crate) struct Reader {
    state: Mutex<Option<sdk::read::Reader>>,
    /// Set at a close, so a `__next__` that waits on another thread ends.
    closed: AtomicBool,
    /// The process that opened the reader.
    process: u32,
    places: Vec<Place>,
    /// The type of each place that is the first place of its name, else `None`: a
    /// frame gives each name once.
    firsts: Vec<Option<Scalar>>,
    /// The name of each place, which each frame shares.
    names: Vec<Py<PyString>>,
    loss: Loss,
    #[cfg(feature = "sim")]
    part: crate::sim::Part,
}

/// What `__next__` copies out of a frame of `sdk` before it makes a Python object.
struct Copied {
    count: u32,
    /// The place of each series of the frame, and its samples.
    series: Vec<(usize, Samples)>,
    /// The place of each series with lost seqs, and those seqs.
    lost: Vec<(usize, Range<u64>)>,
    #[cfg(feature = "sim")]
    began: std::time::Instant,
}

impl Reader {
    /// The Python reader of `reader`, opened in `process`. Raises `TypeError`, and
    /// drops `reader`, for a place of a type that is not a scalar.
    pub(crate) fn new(
        py: Python<'_>,
        reader: sdk::read::Reader,
        process: u32,
        loss: Loss,
    ) -> PyResult<Self> {
        let places = reader.places().to_vec();
        let mut firsts = Vec::with_capacity(places.len());
        for (
            place,
            Place {
                name, data_type, ..
            },
        ) in places.iter().enumerate()
        {
            let Type::Scalar(scalar) = *data_type else {
                return Err(PyTypeError::new_err(format!(
                    "{name} holds {data_type}, which the reader gives no series of"
                )));
            };
            let first = places[..place].iter().all(|other| other.name != *name);
            firsts.push(first.then_some(scalar));
        }
        let names = places
            .iter()
            .map(|place| PyString::intern(py, place.name.as_str()).unbind())
            .collect();
        Ok(Self {
            state: Mutex::new(Some(reader)),
            closed: AtomicBool::new(false),
            process,
            places,
            firsts,
            names,
            loss,
            #[cfg(feature = "sim")]
            part: crate::sim::Part::default(),
        })
    }

    /// The reader, which it waits for with other Python threads running. No Python
    /// object may be made or dropped while the guard lives, as a `__del__` that the
    /// garbage collector runs can call the reader.
    fn lock(&self, py: Python<'_>) -> MutexGuard<'_, Option<sdk::read::Reader>> {
        self.state
            .lock_py_attached(py)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The next frame, copied, or `None` after a close. Runs the signal handlers at
    /// least each [`TICK`] while it waits, with other Python threads running.
    fn copied(&self, py: Python<'_>) -> PyResult<Option<Copied>> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            let next = {
                let mut reader = self.lock(py);
                let Some(reader) = reader.as_mut() else {
                    return Ok(None);
                };
                py.detach(|| {
                    let frame = reader.next_within(TICK)?;
                    Ok(frame.map(|frame| self.copy(frame)))
                })
            };
            match next {
                Ok(Some(copied)) => return Ok(Some(copied)),
                Ok(None) => py.check_signals()?,
                Err(e) => return Err(raised(py, &e)),
            }
        }
    }

    /// The samples and the lost seqs of each name of `frame`.
    fn copy(&self, frame: &sdk::read::Frame) -> Copied {
        #[cfg(feature = "sim")]
        let began = crate::sim::Part::start();
        let mut series = Vec::with_capacity(self.firsts.len());
        let mut lost = Vec::new();
        for (place, scalar) in self.firsts.iter().enumerate() {
            let (Some(scalar), Some(bytes)) = (scalar, frame.series(place)) else {
                continue;
            };
            series.push((place, Samples::new(*scalar, bytes)));
            let seqs = frame.lost(place);
            if !seqs.is_empty() {
                lost.push((place, seqs));
            }
        }
        Copied {
            count: frame.count(),
            series,
            lost,
            #[cfg(feature = "sim")]
            began,
        }
    }

    /// The Python frame of `copied`, after it does what [`Loss`] says with its loss.
    fn frame(&self, py: Python<'_>, copied: Copied) -> PyResult<Py<Frame>> {
        let series = PyDict::new(py);
        for (place, samples) in copied.series {
            series.set_item(self.names[place].bind(py), samples.array(py)?)?;
        }
        let lost: Vec<_> = copied
            .lost
            .into_iter()
            .map(|(place, seqs)| (self.names[place].clone_ref(py), seqs))
            .collect();
        let frame = Py::new(
            py,
            Frame {
                count: copied.count,
                series: series.unbind(),
                lost,
            },
        )?;
        let lost = &frame.get().lost;
        if lost.is_empty() {
            #[cfg(feature = "sim")]
            self.part.mark(copied.began);
            return Ok(frame);
        }
        if self.loss == Loss::Raise {
            self.close(py);
        }
        let lost = lost.iter().map(|(name, seqs)| {
            (name.bind(py), u128::from(seqs.start)..u128::from(seqs.end))
        });
        self.loss
            .act(py, "the reader lost seqs", lost, frame.bind(py).as_any())?;
        Ok(frame)
    }
}

#[pymethods]
impl Reader {
    /// The name and the type of each place, in place order, the type as the spec
    /// writes it (`f64`, `timestamp`).
    #[getter]
    fn places<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        crate::places(py, &self.places)
    }

    /// The nanoseconds from the return of the frame of the last call of `__next__` that
    /// gave one with no loss, to just before that call returned, or 0 before one. Only
    /// in a build with the feature `sim`, for the bench.
    #[cfg(feature = "sim")]
    #[getter]
    fn _part(&self) -> u64 {
        self.part.nanos()
    }

    fn __iter__(this: Py<Self>) -> Py<Self> {
        this
    }

    /// Blocks until the next frame comes, with other Python threads running. It runs
    /// the signal handlers (Ctrl-C) at least each 100 ms while it waits for a frame,
    /// and none while a `__next__` of another thread holds the reader, so read a reader
    /// on one thread.
    ///
    /// Raises `StopIteration` after a close; `DataLost` for a frame after a loss with
    /// `on_loss="raise"`, after which the reader is closed; `Stopped` with `unknown`
    /// when a channel was removed; `ConnectionError` when the session failed, or a
    /// frame of the node does not decode or is over the read budget of the client;
    /// `RuntimeError` when the client closed or in a forked process; and
    /// `PanicException` when the client's thread panicked. After `Stopped`,
    /// `ConnectionError`, or `RuntimeError` of a closed client, each later call raises
    /// it again.
    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<Frame>>> {
        owned(self.process)?;
        self.copied(py)?
            .map(|copied| self.frame(py, copied))
            .transpose()
    }

    /// Ends the reader. A `__next__` that waits on another thread raises
    /// `StopIteration` within 100 ms. A close after a close gives `None`, and in a
    /// forked process it gives `None` and ends nothing.
    fn close(&self, py: Python<'_>) {
        if process::id() != self.process {
            return;
        }
        self.closed.store(true, Ordering::Release);
        let reader = self.lock(py).take();
        drop(reader);
    }

    fn __enter__(this: Py<Self>) -> Py<Self> {
        this
    }

    /// Closes the reader.
    #[pyo3(signature = (_kind, _raised, _traceback))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _kind: &Bound<'_, PyAny>,
        _raised: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) {
        self.close(py);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if process::id() != self.process {
            let reader = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
            #[expect(
                clippy::mem_forget,
                reason = "the drop of the reader sends to the parent's client thread, \
                          which this process does not have"
            )]
            mem::forget(reader.take());
        }
    }
}

/// The Python exception of `error`, an error of a reader: an error of its pool is of
/// the frames of the node, so it raises `ConnectionError`.
pub(crate) fn raised(py: Python<'_>, error: &sdk::Error) -> PyErr {
    match error {
        sdk::Error::Stream(program::Error::Pool(_)) => {
            PyConnectionError::new_err(error.to_string())
        }
        error => crate::error(py, error),
    }
}

/// One frame of a reader: a read-only mapping (`collections.abc.Mapping`) from the name
/// of each channel of the frame, the index included, to its series, `count` samples
/// each: `frame["pt_1"]`, `"pt_1" in frame`, `for name in frame`, and `len(frame)`, the
/// count of its channels. A channel that did not move in the frame is not a key, and a
/// name given twice is one key. Each series is a new one-dimensional `numpy.ndarray`
/// that the program owns, in native byte order: `bool`, `int8` to `uint64`, `float32`,
/// `float64`, `datetime64[ns]` for `timestamp`, `timedelta64[ns]` for `duration`, and
/// `object` for `uuid`, with one `uuid.UUID` for each sample. A later frame does not
/// change it.
#[pyclass(frozen, mapping, module = "foundation")]
pub(crate) struct Frame {
    count: u32,
    /// The series of each name.
    series: Py<PyDict>,
    /// Each name with lost seqs, and those seqs.
    lost: Vec<(Py<PyString>, Range<u64>)>,
}

#[pymethods]
impl Frame {
    /// The samples of each series.
    #[getter]
    fn count(&self) -> u32 {
        self.count
    }

    /// A `dict` from each channel of the frame whose index lost seqs after the last
    /// series of the channel with a sample, up to this frame, to the `range` of those
    /// seqs: `len` of it is their count. Empty when nothing was lost, and always for a
    /// latest reader.
    #[getter]
    fn lost<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let lost = PyDict::new(py);
        for (name, seqs) in &self.lost {
            let seqs = u128::from(seqs.start)..u128::from(seqs.end);
            lost.set_item(name, crate::range(py, seqs)?)?;
        }
        Ok(lost)
    }

    fn __getitem__<'py>(
        &self,
        py: Python<'py>,
        name: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.series
            .bind(py)
            .get_item(name)?
            .ok_or_else(|| PyKeyError::new_err(name.clone().unbind()))
    }

    fn __contains__(&self, py: Python<'_>, name: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.series.bind(py).contains(name)
    }

    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(self.series.bind(py).try_iter()?.into_any())
    }

    fn __len__(&self, py: Python<'_>) -> usize {
        self.series.bind(py).len()
    }

    /// The names of the frame, as `Mapping.keys` gives them.
    fn keys<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.series.bind(py).call_method0("keys")
    }

    /// The series of the frame, as `Mapping.values` gives them.
    fn values<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.series.bind(py).call_method0("values")
    }

    /// The pairs of each name and its series, as `Mapping.items` gives them.
    fn items<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.series.bind(py).call_method0("items")
    }

    /// The series of `name`, or `default` when the frame has no series of it.
    #[pyo3(signature = (name, default=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        name: &Bound<'py, PyAny>,
        default: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let series = self.series.bind(py).get_item(name)?;
        Ok(series
            .or(default)
            .unwrap_or_else(|| py.None().into_bound(py)))
    }
}

/// The samples of a series, of the Rust type of the numpy dtype of its array.
enum Samples {
    Bool(Vec<bool>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Stamp(Vec<Datetime<Nanoseconds>>),
    Span(Vec<Timedelta<Nanoseconds>>),
    /// The `int` of each `uuid.UUID`.
    Uuid(Vec<u128>),
}

impl Samples {
    /// The samples of `bytes`, a series of `scalar` in its raw form.
    fn new(scalar: Scalar, bytes: &[u8]) -> Self {
        match scalar {
            Scalar::Bool => Self::Bool(bytes.iter().map(|&byte| byte != 0).collect()),
            Scalar::I8 => Self::I8(decoded(bytes, i8::from_le_bytes)),
            Scalar::I16 => Self::I16(decoded(bytes, i16::from_le_bytes)),
            Scalar::I32 => Self::I32(decoded(bytes, i32::from_le_bytes)),
            Scalar::I64 => Self::I64(decoded(bytes, i64::from_le_bytes)),
            Scalar::U8 => Self::U8(bytes.to_vec()),
            Scalar::U16 => Self::U16(decoded(bytes, u16::from_le_bytes)),
            Scalar::U32 => Self::U32(decoded(bytes, u32::from_le_bytes)),
            Scalar::U64 => Self::U64(decoded(bytes, u64::from_le_bytes)),
            Scalar::F32 => Self::F32(decoded(bytes, f32::from_le_bytes)),
            Scalar::F64 => Self::F64(decoded(bytes, f64::from_le_bytes)),
            Scalar::Stamp => Self::Stamp(decoded(bytes, |raw| {
                Datetime::from(i64::from_le_bytes(raw))
            })),
            Scalar::Span => Self::Span(decoded(bytes, |raw| {
                Timedelta::from(i64::from_le_bytes(raw))
            })),
            Scalar::Uuid => Self::Uuid(decoded(bytes, u128::from_be_bytes)),
        }
    }

    /// The array of the samples, which takes them with no copy, or makes a `uuid.UUID`
    /// of each `uuid`.
    fn array(self, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        Ok(match self {
            Self::Bool(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::I8(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::I16(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::I32(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::I64(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::U8(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::U16(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::U32(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::U64(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::F32(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::F64(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::Stamp(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::Span(samples) => PyArray1::from_vec(py, samples).into_any(),
            Self::Uuid(samples) => {
                let class = py.import("uuid")?.getattr("UUID")?;
                let int = PyDict::new(py);
                let keys = samples
                    .into_iter()
                    .map(|sample| {
                        int.set_item("int", sample)?;
                        Ok(class.call((), Some(&int))?.unbind())
                    })
                    .collect::<PyResult<Vec<_>>>()?;
                PyArray1::from_vec(py, keys).into_any()
            }
        })
    }
}

/// Each sample of `bytes`, `N` bytes each, as `sample` of its bytes.
fn decoded<T, const N: usize>(bytes: &[u8], sample: impl Fn([u8; N]) -> T) -> Vec<T> {
    let (samples, rest) = bytes.as_chunks::<N>();
    assert!(rest.is_empty(), "invariant: a series holds whole samples");
    samples.iter().map(|raw| sample(*raw)).collect()
}
