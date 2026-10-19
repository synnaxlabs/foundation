//! `foundation.Writer`: a writer of `sdk`, for Python.

use std::fmt::Display;
use std::mem;
use std::ops::Range;
use std::process;
use std::sync::{Mutex, MutexGuard, PoisonError};

use numpy::datetime::{Datetime, Timedelta, units::Nanoseconds};
use numpy::{
    Element, PyArray1, PyArrayDescrMethods, PyArrayMethods, PyUntypedArray,
    PyUntypedArrayMethods,
};
use pyo3::exceptions::{
    PyKeyError, PyOverflowError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyBool, PyDict, PyInt, PyString, PyTuple};
use sdk::write::{Outcome, Place, Refusal};
use types::sample::{Scalar, Type, Value};
use types::time::{Span, Stamp};
use types::{frame, hash};

use crate::{DataLost, Rejected, error, owned};

/// What a write does with a frame that the node took no room for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loss {
    /// Log a warning on the `foundation` logger.
    Warn,
    /// Raise `DataLost`.
    Raise,
    /// Do nothing.
    Ignore,
}

impl Loss {
    /// The loss of the argument `on_loss`.
    pub(crate) fn parse(on_loss: &str) -> PyResult<Self> {
        match on_loss {
            "warn" => Ok(Self::Warn),
            "raise" => Ok(Self::Raise),
            "ignore" => Ok(Self::Ignore),
            other => Err(PyValueError::new_err(format!(
                "on_loss is {other:?}, not \"warn\", \"raise\", or \"ignore\""
            ))),
        }
    }
}

/// A writer of channels of one index at its home, which `Client.open_writer` opens. A
/// `with` block closes it at its end. Its calls take turns to send: a call waits, with
/// other Python threads running, while another call of the writer sends. A value or a
/// logging handler that a call runs can call the writer. A process that forks from the
/// program does not own the writer: in it, each call but a close raises
/// `RuntimeError`, and a close gives `None` and ends nothing.
#[pyclass(frozen, module = "foundation")]
pub(crate) struct Writer {
    state: Mutex<Option<State>>,
    /// The process that opened the writer.
    process: u32,
    places: Vec<Place>,
    /// The place of each channel name: the first place of a name that two places
    /// hold.
    named: hash::Map<String, usize>,
    index: usize,
    /// The most bytes of one frame, which the series of a call do not go over.
    bytes_max: usize,
    loss: Loss,
    #[cfg(feature = "sim")]
    part: crate::sim::Part,
}

/// The writer of `sdk` and the buffers that the last call filled.
struct State {
    writer: sdk::write::Writer,
    kept: Buffers,
}

/// The buffers that a call fills, kept so a warm call allocates only the list of its
/// series.
#[derive(Default)]
struct Buffers {
    values: Vec<(usize, Value)>,
    bytes: Vec<u8>,
    /// The place of each series of `write_series`, and its bytes in `bytes`.
    series: Vec<(usize, Range<usize>)>,
}

impl Writer {
    /// The Python writer of `writer`, opened in `process`.
    pub(crate) fn new(writer: sdk::write::Writer, process: u32, loss: Loss) -> Self {
        let mut named = hash::Map::default();
        for (place, Place { name, .. }) in writer.places().iter().enumerate() {
            named.entry(name.as_str().to_owned()).or_insert(place);
        }
        Self {
            places: writer.places().to_vec(),
            named,
            index: writer.index(),
            bytes_max: writer.bytes_max(),
            state: Mutex::new(Some(State {
                writer,
                kept: Buffers::default(),
            })),
            process,
            loss,
            #[cfg(feature = "sim")]
            part: crate::sim::Part::default(),
        }
    }

    /// The state, which it waits for with other Python threads running. No Python code
    /// may run while the guard lives, as that code can call the writer.
    fn lock(&self, py: Python<'_>) -> MutexGuard<'_, Option<State>> {
        self.state
            .lock_py_attached(py)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls `fill` with no lock held on the kept buffers, or on empty ones while
    /// another call fills them, and gives the buffers and what `fill` gives. When
    /// `fill` raises, keeps the buffers for the next call. Raises `RuntimeError` in a
    /// process that the program forked, without the lock that the fork can copy as
    /// held, and after a close.
    fn fill<T>(
        &self,
        py: Python<'_>,
        fill: impl FnOnce(&mut Buffers) -> PyResult<T>,
    ) -> PyResult<(Buffers, T)> {
        owned(self.process)?;
        let mut buffers = mem::take(&mut opened(&mut self.lock(py))?.kept);
        match fill(&mut buffers) {
            Ok(filled) => Ok((buffers, filled)),
            Err(e) => {
                if let Some(state) = self.lock(py).as_mut() {
                    state.kept = buffers;
                }
                Err(e)
            }
        }
    }

    /// Calls `send` on the `sdk` writer with `buffers` and the GIL released, keeps
    /// `buffers` for the next call, and then maps the outcome as [`Self::outcome`]
    /// says. `places` gives the places of the frame. Raises `RuntimeError` after a
    /// close.
    fn send<F>(
        &self,
        py: Python<'_>,
        buffers: Buffers,
        send: F,
        places: impl FnOnce(&Buffers) -> Vec<usize>,
    ) -> PyResult<()>
    where
        F: FnOnce(&mut sdk::write::Writer, &Buffers) -> Result<Outcome, sdk::Error>
            + Send,
    {
        let (outcome, lost) = {
            let mut state = self.lock(py);
            let state = opened(&mut state)?;
            let writer = &mut state.writer;
            let outcome = py.detach(|| send(writer, &buffers));
            let lost = match outcome {
                Ok(Outcome::Lost { .. }) => places(&buffers),
                _ => Vec::new(),
            };
            state.kept = buffers;
            (outcome, lost)
        };
        self.outcome(py, outcome, lost)
    }

    /// The place of the key `key`: a channel name or a place.
    fn place(&self, key: &Bound<'_, PyAny>) -> PyResult<usize> {
        if let Ok(name) = key.cast::<PyString>() {
            let name = name.to_str()?;
            return self.named.get(name).copied().ok_or_else(|| {
                let names: Vec<_> =
                    self.places.iter().map(|p| p.name.as_str()).collect();
                PyKeyError::new_err(format!(
                    "the writer has no place {name}: its places are {}",
                    names.join(", ")
                ))
            });
        }
        if let Ok(place) = key.cast::<PyInt>() {
            return place
                .extract::<usize>()
                .ok()
                .filter(|&place| place < self.places.len())
                .ok_or_else(|| {
                    PyValueError::new_err(format!(
                        "the writer has no place {place}: it has {} places",
                        self.places.len()
                    ))
                });
        }
        Err(PyTypeError::new_err(format!(
            "a key is a channel name or a place, not {}",
            key.get_type().name()?
        )))
    }

    /// The one place other than the index, for a value with no key.
    fn only(&self) -> PyResult<usize> {
        match self.places.len() {
            2 => Ok(1 - self.index),
            count => Err(PyTypeError::new_err(format!(
                "the writer has {} places other than the index: pass a dict from each \
                 channel name to its value",
                count - 1
            ))),
        }
    }

    /// Returns `None` for a frame that the node took, and raises for each other
    /// outcome, or the exception of `error`. `places` are the places of a lost frame.
    fn outcome(
        &self,
        py: Python<'_>,
        outcome: Result<Outcome, sdk::Error>,
        places: Vec<usize>,
    ) -> PyResult<()> {
        match outcome.map_err(|e| error(py, &e))? {
            Outcome::Taken { .. } => Ok(()),
            Outcome::Refused(refusal) => {
                let (code, text) = rejection(refusal);
                let rejected =
                    Rejected::new_err(format!("the node refused the frame: {text}"));
                rejected.value(py).setattr("code", code)?;
                Err(rejected)
            }
            Outcome::Lost { range } => self.lost(py, range, places),
        }
    }

    /// Does what [`Loss`] says with the loss of the samples `range` of the series of
    /// `places`.
    fn lost(
        &self,
        py: Python<'_>,
        range: frame::Range,
        mut places: Vec<usize>,
    ) -> PyResult<()> {
        if self.loss == Loss::Ignore {
            return Ok(());
        }
        places.sort_unstable();
        places.dedup();
        let names: Vec<_> = places
            .iter()
            .map(|&place| format!("{} ({})", self.places[place].name, range.count))
            .collect();
        let text = format!("samples were lost: {}", names.join(", "));
        if self.loss == Loss::Warn {
            py.import("logging")?
                .call_method1("getLogger", ("foundation",))?
                .call_method1("warning", (text,))?;
            return Ok(());
        }
        let end = u128::from(range.seq) + u128::from(range.count);
        let seqs = py
            .import("builtins")?
            .getattr("range")?
            .call1((range.seq, end))?;
        let lost = PyDict::new(py);
        for place in places {
            lost.set_item(self.places[place].name.as_str(), &seqs)?;
        }
        let raised = DataLost::new_err(text);
        raised.value(py).setattr("seqs", lost)?;
        Err(raised)
    }
}

#[pymethods]
impl Writer {
    /// The name and the type of each place, in place order, the type as the spec
    /// writes it (`f64`, `timestamp`).
    #[getter]
    fn places<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let places = self
            .places
            .iter()
            .map(|place| (place.name.as_str(), place.data_type.to_string()));
        PyTuple::new(py, places)
    }

    /// The place of the index, whose samples are the stamps of each frame.
    #[getter]
    fn index(&self) -> usize {
        self.index
    }

    /// The nanoseconds from the start of the last call that reached the `sdk` call to
    /// just before that call, or 0 before one. Only in a build with the feature `sim`,
    /// for the bench.
    #[cfg(feature = "sim")]
    #[getter]
    fn _part(&self) -> u64 {
        self.part.nanos()
    }

    /// The bytes that the kept buffer of series holds room for. Only in a build with
    /// the feature `sim`.
    #[cfg(feature = "sim")]
    #[getter]
    fn _kept(&self, py: Python<'_>) -> usize {
        self.lock(py)
            .as_ref()
            .map_or(0, |state| state.kept.bytes.capacity())
    }

    /// Writes one frame of one sample of each value, and blocks until the node took
    /// it. `values` is a `dict` from a channel name or a place to a value, or one value
    /// when the writer has one place other than the index. A value at the index is the
    /// stamp of the frame, in nanoseconds since the UNIX epoch; with none, the node
    /// stamps the frame. A value is a `bool` or a `numpy.bool_` for `bool`, an `int`
    /// (or an object with `__index__`) for an integer type and for `timestamp` and
    /// `duration` (nanoseconds), a `float` or an `int` for `f32` and `f64`, and a
    /// `uuid.UUID` for `uuid`. The call reads `values` after it waits for a call of
    /// another thread that sends. A change of `values` after that read is not in the
    /// frame. Other Python threads run while it waits.
    ///
    /// Raises `TypeError` for a value that is not of its place's type, a place of a
    /// type that is not a scalar, a key that is not a name or a place, or one value
    /// with no key for a writer of more places; `OverflowError` for a number outside
    /// its type; `KeyError` for a name of no place; and `ValueError` for no value, a
    /// place past the places, or two values of one channel. With each of these, nothing
    /// was sent and the writer goes on. Raises `Rejected` when the node refused the
    /// frame, and `DataLost` for a lost frame with `on_loss="raise"`; the writer goes
    /// on after both. Raises `Stopped` when the node stopped the stream, and
    /// `ConnectionError` when the session failed, after which each call raises it
    /// again; `RuntimeError` after a close or in a forked process; and
    /// `PanicException` when the client's thread panicked.
    fn write(&self, py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<()> {
        #[cfg(feature = "sim")]
        let began = crate::sim::Part::start();
        let (buffers, ()) = self.fill(py, |buffers| {
            buffers.values.clear();
            if let Ok(values) = values.cast::<PyDict>() {
                // A copy, as a value can change the dict.
                for (key, object) in values.copy()?.iter() {
                    let place = self.place(&key)?;
                    let value = value(&self.places[place], &object)?;
                    buffers.values.push((place, value));
                }
            } else {
                let place = self.only()?;
                let value = value(&self.places[place], values)?;
                buffers.values.push((place, value));
            }
            Ok(())
        })?;
        let send = |writer: &mut sdk::write::Writer, buffers: &Buffers| {
            #[cfg(feature = "sim")]
            self.part.mark(began);
            writer.write(&buffers.values)
        };
        self.send(py, buffers, send, |buffers| {
            let index = (!buffers.values.is_empty()).then_some(self.index);
            let places = buffers.values.iter().map(|&(place, _)| place);
            places.chain(index).collect()
        })
    }

    /// Writes one frame of each series of `series`, a `dict` from a channel name or a
    /// place to a one-dimensional `numpy.ndarray`, and blocks until the node took it.
    /// Each array has the same length, and the series of the index holds the stamps.
    /// Each array has the dtype of its place in native byte order: `bool`, `int8` to
    /// `uint64`, `float32`, `float64`, and `int64` or `datetime64[ns]` for
    /// `timestamp`, `int64` or `timedelta64[ns]` for `duration`. The writer casts
    /// nothing, and copies each array before the send, in which other Python threads
    /// run. The call reads `series` after it waits for a call of another thread that
    /// sends. A change of `series` after that read is not in the frame.
    ///
    /// Raises `TypeError` for a value that is not an array of one dimension of its
    /// place's dtype (its text names the place, the dtypes that it takes, the dtype
    /// given, and the fix, such as `pass a.astype("float64")`), or a place of `uuid` or
    /// of a type that is not a scalar, and `ValueError` for arrays of different
    /// lengths, an array of more than 4294967295 samples, series over the bytes of one
    /// frame (before their copy), no series, no series of the index, a frame over the
    /// largest block, or an array that another call writes to.
    /// With each of these, nothing was sent and the writer goes on. Raises each other
    /// error as `write` raises it.
    fn write_series(&self, py: Python<'_>, series: &Bound<'_, PyDict>) -> PyResult<()> {
        #[cfg(feature = "sim")]
        let began = crate::sim::Part::start();
        let (buffers, samples) = self.fill(py, |buffers| {
            buffers.bytes.clear();
            buffers.series.clear();
            let mut count: Option<(usize, u32)> = None;
            // A copy, as on Python 3.11 an allocation in the loop, such as the
            // arguments of the `view` of a `bool` array, can run the garbage collector
            // and so a `__del__` that changes the dict.
            for (key, array) in series.copy()?.iter() {
                let place = self.place(&key)?;
                let start = buffers.bytes.len();
                let (bytes, max) = (&mut buffers.bytes, self.bytes_max);
                let length = copy(&self.places[place], &array, bytes, max)?;
                match count {
                    Some((first, samples)) if samples != length => {
                        return Err(PyValueError::new_err(format!(
                            "the series of {} holds {length} samples, not the \
                             {samples} of {}",
                            self.places[place].name, self.places[first].name
                        )));
                    }
                    Some(_) => {}
                    None => count = Some((place, length)),
                }
                buffers.series.push((place, start..buffers.bytes.len()));
            }
            Ok(count.map_or(0, |(_, samples)| samples))
        })?;
        let send = move |writer: &mut sdk::write::Writer, buffers: &Buffers| {
            let Buffers { bytes, series, .. } = buffers;
            let parts: Vec<_> = series
                .iter()
                .map(|(place, range)| (*place, &bytes[range.clone()]))
                .collect();
            #[cfg(feature = "sim")]
            self.part.mark(began);
            writer.write_series(samples, &parts)
        };
        self.send(py, buffers, send, |buffers| {
            buffers.series.iter().map(|&(place, _)| place).collect()
        })
    }

    /// Finishes the stream, and blocks until each frame that the node took is on disk
    /// and the node finished the stream. Control ends then. A close waits for a call
    /// that sends, with other Python threads running. A call that a close ends before
    /// its send raises `RuntimeError`. A close after a close gives `None`, and in a
    /// forked process it gives `None` and ends nothing.
    ///
    /// Raises `Stopped` when the node stopped the stream, `ConnectionError` when the
    /// session failed, `RuntimeError` when the client closed, and `PanicException`
    /// when the client's thread panicked.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if process::id() != self.process {
            return Ok(());
        }
        let state = self.lock(py).take();
        state.map_or(Ok(()), |state| {
            py.detach(|| state.writer.close())
                .map_err(|e| error(py, &e))
        })
    }

    fn __enter__(this: Py<Self>) -> Py<Self> {
        this
    }

    /// Closes the writer. When the block raised, an error of the close is a note on
    /// the error of the block; else it raises.
    #[pyo3(signature = (_kind, raised, _traceback))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _kind: &Bound<'_, PyAny>,
        raised: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        crate::exit(self.close(py), raised)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if process::id() != self.process {
            let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
            #[expect(
                clippy::mem_forget,
                reason = "the drop of the writer sends to the parent's client thread, \
                          which this process does not have"
            )]
            mem::forget(state.take());
        }
    }
}

/// The state of an open writer. Raises `RuntimeError` after a close.
fn opened(state: &mut Option<State>) -> PyResult<&mut State> {
    state
        .as_mut()
        .ok_or_else(|| PyRuntimeError::new_err("the writer is closed"))
}

/// The code and the text of the frame refusal `refusal`.
fn rejection(refusal: Refusal) -> (&'static str, &'static str) {
    match refusal {
        Refusal::Waiting => ("waiting", "another writer holds control"),
        Refusal::Reserved => (
            "reserved",
            "control is held for a writer from before a restart",
        ),
        Refusal::Expired => (
            "expired",
            "the writer's control lease ran out: open the writer again",
        ),
        Refusal::Order => ("order", "a stamp is not after the stamp before it"),
        Refusal::Codec => ("codec", "a series is not a series of its channel's type"),
        Refusal::Full => (
            "full",
            "a backfill frame found no room in the log, or the frame found no block of \
             the node's pool: write it again later",
        ),
        Refusal::Large => ("large", "the frame is too large for one write: split it"),
    }
}

/// The value of `object` for `place`.
fn value(place: &Place, object: &Bound<'_, PyAny>) -> PyResult<Value> {
    let Type::Scalar(scalar) = place.data_type else {
        return Err(PyTypeError::new_err(format!(
            "{} holds {}, which the writer takes no value of",
            place.name, place.data_type
        )));
    };
    let numpy = numpy::dtype::<bool>(object.py()).typeobj();
    let boolean = object.is_instance_of::<PyBool>() || object.is_instance(&numpy)?;
    if boolean != (scalar == Scalar::Bool) {
        return Err(wrong(place, object)?);
    }
    if boolean {
        return Ok(Value::Bool(object.is_truthy()?));
    }
    Ok(match scalar {
        Scalar::I8 => Value::I8(int(place, object, i8::MIN, i8::MAX)?),
        Scalar::I16 => Value::I16(int(place, object, i16::MIN, i16::MAX)?),
        Scalar::I32 => Value::I32(int(place, object, i32::MIN, i32::MAX)?),
        Scalar::I64 => Value::I64(int(place, object, i64::MIN, i64::MAX)?),
        Scalar::U8 => Value::U8(int(place, object, u8::MIN, u8::MAX)?),
        Scalar::U16 => Value::U16(int(place, object, u16::MIN, u16::MAX)?),
        Scalar::U32 => Value::U32(int(place, object, u32::MIN, u32::MAX)?),
        Scalar::U64 => Value::U64(int(place, object, u64::MIN, u64::MAX)?),
        Scalar::Stamp => {
            Value::Stamp(Stamp::from_nanos(int(place, object, i64::MIN, i64::MAX)?))
        }
        Scalar::Span => {
            Value::Span(Span::from_nanos(int(place, object, i64::MIN, i64::MAX)?))
        }
        Scalar::F32 | Scalar::F64 => {
            let float = match object.extract::<f64>() {
                Ok(float) => float,
                Err(e) if e.is_instance_of::<PyOverflowError>(object.py()) => {
                    return Err(outside(place, object, "its range"));
                }
                Err(_) => return Err(wrong(place, object)?),
            };
            if scalar == Scalar::F64 {
                Value::F64(float)
            } else {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the cast rounds to the nearest f32, and a finite value \
                              that it makes infinite raises"
                )]
                let narrow = float as f32;
                if narrow.is_infinite() && float.is_finite() {
                    return Err(outside(place, object, "its range"));
                }
                Value::F32(narrow)
            }
        }
        Scalar::Uuid => {
            let uuid = object.py().import("uuid")?.getattr("UUID")?;
            if !object.is_instance(&uuid)? {
                return Err(wrong(place, object)?);
            }
            Value::Uuid(object.getattr("int")?.extract::<u128>()?.to_be_bytes())
        }
        Scalar::Bool => unreachable!("a bool returns above"),
    })
}

/// The integer of `object`, from `min` to `max`.
fn int<T>(place: &Place, object: &Bound<'_, PyAny>, min: T, max: T) -> PyResult<T>
where
    T: TryFrom<i128> + Display + Copy,
{
    let wide = match object.extract::<i128>() {
        Ok(wide) => wide,
        Err(e) if e.is_instance_of::<PyOverflowError>(object.py()) => i128::MAX,
        Err(_) => return Err(wrong(place, object)?),
    };
    T::try_from(wide)
        .map_err(|_outside| outside(place, object, &format!("{min} to {max}")))
}

/// The `TypeError` of `object`, a value of another type, for `place`.
fn wrong(place: &Place, object: &Bound<'_, PyAny>) -> PyResult<PyErr> {
    Ok(PyTypeError::new_err(format!(
        "{} holds {}, not {}",
        place.name,
        place.data_type,
        object.get_type().name()?
    )))
}

/// The `OverflowError` of `object`, a number outside `range`, for `place`.
fn outside(place: &Place, object: &Bound<'_, PyAny>, range: &str) -> PyErr {
    let number = object
        .str()
        .map_or_else(|_| "the value".to_owned(), |s| s.to_string());
    PyOverflowError::new_err(format!(
        "{} holds {}, and {number} is outside {range}",
        place.name, place.data_type
    ))
}

/// Copies the array `array` for `place` into `bytes` in the raw form of the place's
/// type, and gives its length. Raises `ValueError`, before any copy, for an array of
/// more samples than one frame holds, or when `bytes` and the array go over `max`
/// bytes.
fn copy(
    place: &Place,
    array: &Bound<'_, PyAny>,
    bytes: &mut Vec<u8>,
    max: usize,
) -> PyResult<u32> {
    let Ok(array) = array.cast::<PyUntypedArray>() else {
        return Err(PyTypeError::new_err(format!(
            "{} takes a numpy.ndarray, not {}",
            place.name,
            array.get_type().name()?
        )));
    };
    if array.ndim() != 1 {
        return Err(PyTypeError::new_err(format!(
            "{} takes an array of one dimension, not {}",
            place.name,
            array.ndim()
        )));
    }
    let Type::Scalar(scalar) = place.data_type else {
        return Err(PyTypeError::new_err(format!(
            "{} holds {}, which the writer takes no series of",
            place.name, place.data_type
        )));
    };
    if scalar == Scalar::Uuid {
        return Err(PyTypeError::new_err(format!(
            "{} holds uuid, which the writer takes no series of",
            place.name
        )));
    }
    let Ok(length) = u32::try_from(array.len()) else {
        return Err(PyValueError::new_err(format!(
            "the series hold {} samples, over the {} of one frame",
            array.len(),
            u32::MAX
        )));
    };
    let end = bytes.len() + array.len() * scalar.width();
    if end > max {
        return Err(PyValueError::new_err(format!(
            "the series hold {} samples, {end} bytes, over the {max} bytes of one frame",
            array.len()
        )));
    }
    if put(scalar, array, bytes)? {
        return Ok(length);
    }
    let dtype = array.dtype();
    let (takes, fix) = match (scalar, dtype.kind()) {
        (Scalar::Stamp, b'M') => ("int64 or datetime64[ns]", "datetime64[ns]"),
        (Scalar::Stamp, _) => ("int64 or datetime64[ns]", "int64"),
        (Scalar::Span, b'm') => ("int64 or timedelta64[ns]", "timedelta64[ns]"),
        (Scalar::Span, _) => ("int64 or timedelta64[ns]", "int64"),
        (Scalar::Bool, _) => ("bool", "bool"),
        (Scalar::I8, _) => ("int8", "int8"),
        (Scalar::I16, _) => ("int16", "int16"),
        (Scalar::I32, _) => ("int32", "int32"),
        (Scalar::I64, _) => ("int64", "int64"),
        (Scalar::U8, _) => ("uint8", "uint8"),
        (Scalar::U16, _) => ("uint16", "uint16"),
        (Scalar::U32, _) => ("uint32", "uint32"),
        (Scalar::U64, _) => ("uint64", "uint64"),
        (Scalar::F32, _) => ("float32", "float32"),
        (Scalar::F64, _) => ("float64", "float64"),
        (Scalar::Uuid, _) => unreachable!("a uuid returns above"),
    };
    Err(dtyped(place, takes, fix, &dtype))
}

/// Copies `array` into `bytes` in the raw form of `scalar` when it is an array of a
/// dtype that `scalar` takes, and gives whether it is.
fn put(
    scalar: Scalar,
    array: &Bound<'_, PyUntypedArray>,
    bytes: &mut Vec<u8>,
) -> PyResult<bool> {
    Ok(match scalar {
        Scalar::Bool => {
            // A bool array can hold a byte other than 0 and 1, which no Rust `bool` is.
            if array.cast::<PyArray1<bool>>().is_err() {
                return Ok(false);
            }
            // `ndarray.view` of the base type, so no method of a subclass runs.
            let py = array.py();
            let ndarray = py.get_type::<PyUntypedArray>();
            let view = ndarray
                .call_method1("view", (array, numpy::dtype::<u8>(py), &ndarray))?;
            raw(view.cast()?, bytes, |byte: u8| Value::Bool(byte != 0))?
        }
        Scalar::I8 => raw(array, bytes, Value::I8)?,
        Scalar::I16 => raw(array, bytes, Value::I16)?,
        Scalar::I32 => raw(array, bytes, Value::I32)?,
        Scalar::I64 => raw(array, bytes, Value::I64)?,
        Scalar::U8 => raw(array, bytes, Value::U8)?,
        Scalar::U16 => raw(array, bytes, Value::U16)?,
        Scalar::U32 => raw(array, bytes, Value::U32)?,
        Scalar::U64 => raw(array, bytes, Value::U64)?,
        Scalar::F32 => raw(array, bytes, Value::F32)?,
        Scalar::F64 => raw(array, bytes, Value::F64)?,
        Scalar::Stamp => {
            raw(array, bytes, |nanos| Value::Stamp(Stamp::from_nanos(nanos)))?
                || raw(array, bytes, |stamp: Datetime<Nanoseconds>| {
                    Value::Stamp(Stamp::from_nanos(stamp.into()))
                })?
        }
        Scalar::Span => {
            raw(array, bytes, |nanos| Value::Span(Span::from_nanos(nanos)))?
                || raw(array, bytes, |span: Timedelta<Nanoseconds>| {
                    Value::Span(Span::from_nanos(span.into()))
                })?
        }
        Scalar::Uuid => unreachable!("copy takes no uuid"),
    })
}

/// The `TypeError` of an array of `dtype` for `place`, which takes the dtypes `takes`,
/// with the cast to `fix`.
fn dtyped(place: &Place, takes: &str, fix: &str, dtype: &Bound<'_, PyAny>) -> PyErr {
    PyTypeError::new_err(format!(
        "{} holds {} and takes an array of {takes}, not {dtype}: pass \
         a.astype(\"{fix}\")",
        place.name, place.data_type
    ))
}

/// Copies `array` into `bytes`, each sample as `value` of it, when it is an array of
/// `T`, and gives whether it is. Raises `ValueError` when another call writes to the
/// array.
#[expect(
    unsafe_code,
    reason = "the `numpy` crate has no sound read of an array that is not aligned"
)]
fn raw<T: Element + Copy>(
    array: &Bound<'_, PyUntypedArray>,
    bytes: &mut Vec<u8>,
    value: impl Fn(T) -> Value,
) -> PyResult<bool> {
    let Ok(array) = array.cast::<PyArray1<T>>() else {
        return Ok(false);
    };
    let array = array
        .try_readonly()
        .map_err(|e| PyValueError::new_err(format!("cannot read the array: {e}")))?;
    let start = bytes.len();
    bytes.resize(start + array.len() * size_of::<T>(), 0);
    #[expect(
        clippy::chunks_exact_to_as_chunks,
        reason = "a const argument cannot name the size of a generic type"
    )]
    let chunks = bytes[start..].chunks_exact_mut(size_of::<T>());
    if let Ok(samples) = array.as_slice() {
        fill(chunks, samples.iter().copied(), value);
        return Ok(true);
    }
    // An array that is not aligned or not contiguous, such as a field of a packed
    // struct, is read at its byte stride, which can be negative.
    let stride = array.strides()[0];
    let mut sample = array.data().cast_const();
    let samples = std::iter::from_fn(|| {
        // SAFETY: the borrow above keeps the array alive and unwritten, and a
        // one-dimensional array holds its `len` samples at `stride` bytes apart from
        // `data`. `chunks` ends the read after the last sample. Each bit pattern is a
        // valid `T` of each `T` that `put` passes, as a `bool` array goes as `u8`.
        let read = unsafe { sample.read_unaligned() };
        sample = sample.wrapping_byte_offset(stride);
        Some(read)
    });
    fill(chunks, samples, value);
    Ok(true)
}

/// Writes each of `samples` as `value` of it into its chunk of `chunks`, and reads
/// no sample past the last chunk.
fn fill<'a, T: Copy>(
    chunks: impl Iterator<Item = &'a mut [u8]>,
    samples: impl Iterator<Item = T>,
    value: impl Fn(T) -> Value,
) {
    for (chunk, sample) in chunks.zip(samples) {
        value(sample).write(chunk);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use pyo3::prelude::*;
    use pyo3::types::PyDict;
    use sdk::write::Place;
    use types::channel;
    use types::sample::{Scalar, Type, Value};

    /// A place of `scalar`.
    fn place(scalar: Scalar) -> Place {
        Place {
            key: channel::Key::from_u128(1),
            data_type: Type::Scalar(scalar),
            name: "plant.a".parse().unwrap(),
        }
    }

    /// Makes `numpy.array` raise for the `array` of the globals, so that `copy` must
    /// read it in place. Other arrays pass, as the tests share one interpreter.
    const GUARD: &CStr = c"
import numpy as np
def guard(real, copied):
    def array(source, *args, **kwargs):
        assert source is not copied, 'numpy.array of the array'
        return real(source, *args, **kwargs)
    return array
np.array = guard(np.array, array)
";

    /// The bytes that `copy` gives for the `array` that `code` makes, for a place of
    /// `scalar`.
    // A call of the private `copy`, as no public seam reads the samples back: the
    // Python suite through `write_series` sees only `None` until a reader comes
    // (#3321), and a read through `program::Session::reader` needs a node process, a
    // session, and a `Writer` built from Rust, with no harness in this crate.
    fn copied(scalar: Scalar, code: &CStr) -> PyResult<Vec<u8>> {
        let place = place(scalar);
        Python::initialize();
        Python::attach(|py| {
            let globals = PyDict::new(py);
            py.run(code, Some(&globals), None)?;
            py.run(GUARD, Some(&globals), None)?;
            let array = globals.get_item("array")?.unwrap();
            let mut bytes = Vec::new();
            super::copy(&place, &array, &mut bytes, usize::MAX)?;
            Ok(bytes)
        })
    }

    // The `// SAFETY:` comment of `raw` relies on this bound, and a read past the
    // array changes no byte that a test on CPython sees.
    #[test]
    fn fill_pulls_no_sample_after_the_last_chunk() {
        let mut bytes = [0_u8; 3 * 8];
        let mut pulled = 0_u8;
        let samples = std::iter::from_fn(|| {
            pulled += 1;
            assert!(pulled <= 3, "a pull after the last chunk");
            Some(f64::from(pulled))
        });
        let chunks = bytes.as_chunks_mut::<8>().0.iter_mut();
        super::fill(chunks.map(|chunk| &mut chunk[..]), samples, Value::F64);
        let floats: Vec<u8> = [1.0_f64, 2.0, 3.0]
            .iter()
            .flat_map(|float| float.to_le_bytes())
            .collect();
        assert_eq!(bytes[..], floats[..]);
    }

    #[test]
    fn copy_writes_each_true_of_a_bool_array_as_1() {
        let code = c"
import numpy as np
array = np.frombuffer(bytes([0, 2, 255]), dtype=np.bool_)
assert list(array) == [False, True, True]
";
        assert_eq!(copied(Scalar::Bool, code).unwrap(), [0, 1, 1]);
        let code = c"
import numpy as np
array = np.array([2, 0], np.uint8).view(bool)
";
        assert_eq!(copied(Scalar::Bool, code).unwrap(), [1, 0]);
    }

    #[test]
    fn copy_reads_each_sample_of_an_array_that_is_not_aligned_or_packed() {
        let floats: Vec<u8> = [1.0_f64, 2.0, 3.0, 4.0]
            .iter()
            .flat_map(|float| float.to_le_bytes())
            .collect();
        let codes = [
            c"
import numpy as np
raw = bytes(1) + np.arange(1, 5, dtype='<f8').tobytes()
array = np.frombuffer(raw, dtype=np.float64, offset=1)
assert not array.flags.aligned
",
            c"
import numpy as np
packed = np.zeros(4, [('b', '<f8'), ('a', 'u1', (4,))])
packed['b'] = np.arange(1, 5)
array = packed['b']
assert array.strides == (12,)
",
            c"
import numpy as np
packed = np.zeros(4, [('a', 'u1'), ('b', '<f8')])
packed['b'] = np.arange(1, 5)
array = packed['b']
assert array.strides == (9,) and not array.flags.aligned
",
            c"
import numpy as np
packed = np.zeros(8, [('a', 'u1'), ('b', '<f8')])
packed['b'] = np.arange(5, -3, -1)
array = packed['b'][4:0:-1]
assert array.strides == (-9,)
",
        ];
        for code in codes {
            assert_eq!(copied(Scalar::F64, code).unwrap(), floats, "{code:?}");
        }
    }

    #[test]
    fn copy_reads_each_sample_of_a_strided_array() {
        let code = c"
import numpy as np
array = np.arange(-3, 5, dtype=np.int16)[::-2][::2]
assert list(array) == [4, 0]
";
        assert_eq!(copied(Scalar::I16, code).unwrap(), [4, 0, 0, 0]);
        let code = c"
import numpy as np
array = np.arange(1, 9, dtype='<f8')[::2]
";
        let floats: Vec<u8> = [1.0_f64, 3.0, 5.0, 7.0]
            .iter()
            .flat_map(|float| float.to_le_bytes())
            .collect();
        assert_eq!(copied(Scalar::F64, code).unwrap(), floats);
    }
}
