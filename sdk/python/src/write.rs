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
use program::write::End;
use pyo3::exceptions::{
    PyKeyError, PyOverflowError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyBool, PyDict, PyInt, PyString, PyTuple};
use sdk::Place;
use sdk::write::{Outcome, Refusal};
use types::frame;
use types::hash;
use types::sample::{Scalar, Type, Value};
use types::time::{Span, Stamp};

use crate::{GroupEnded, Loss, Rejected, Stopped, owned, stop};

/// A writer of channels of any index at their homes, which `Client.open_writer` opens.
/// A `with` block closes it at its end. Its calls take turns to send: a call waits,
/// with other Python threads running, while another call of the writer sends. A value
/// or a logging handler that a call runs can call the writer. A process that forks from
/// the program does not own the writer: in it, each call but a close raises
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
    /// The one place that is not an index, for a value with no key, when the writer
    /// has one.
    only: Option<usize>,
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
    /// The place of each series of `write_series`, its bytes in `bytes`, and its count.
    series: Vec<(usize, Range<usize>, u32)>,
}

impl Writer {
    /// The Python writer of `writer`, opened in `process`.
    pub(crate) fn new(writer: sdk::write::Writer, process: u32, loss: Loss) -> Self {
        let places = writer.places();
        let named = named(places);
        let mut values = (0..places.len()).filter(|&place| !indexed(places, place));
        let only = values.next().filter(|_| values.next().is_none());
        Self {
            places: places.to_vec(),
            named,
            only,
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
    /// another call fills them, and gives the buffers. When `fill` raises, keeps the
    /// buffers for the next call. Raises `RuntimeError` in a process that the program
    /// forked, without the lock that the fork can copy as held, and after a close.
    fn fill(
        &self,
        py: Python<'_>,
        fill: impl FnOnce(&mut Buffers) -> PyResult<()>,
    ) -> PyResult<Buffers> {
        owned(self.process)?;
        let mut buffers = mem::take(&mut opened(&mut self.lock(py))?.kept);
        match fill(&mut buffers) {
            Ok(()) => Ok(buffers),
            Err(e) => {
                if let Some(state) = self.lock(py).as_mut() {
                    state.kept = buffers;
                }
                Err(e)
            }
        }
    }

    /// Calls `send` on the `sdk` writer with `buffers` and the GIL released, keeps
    /// `buffers` for the next call, and then returns `None` when the node took each
    /// group, answers as [`answer`] says for the outcomes of a frame that it did not
    /// take whole, or raises the exception of the error as [`raised`] maps it. `places`
    /// gives the places of the items of the frame. Raises `RuntimeError` after a close.
    fn send<F>(
        &self,
        py: Python<'_>,
        buffers: Buffers,
        send: F,
        places: impl FnOnce(&Buffers) -> Vec<usize>,
    ) -> PyResult<()>
    where
        F: FnOnce(&mut sdk::write::Writer, &Buffers) -> Untaken + Send,
    {
        let (untaken, frame) = {
            let mut state = self.lock(py);
            let state = opened(&mut state)?;
            let writer = &mut state.writer;
            let untaken = py.detach(|| send(writer, &buffers));
            let frame = match untaken {
                Ok(Some(_)) => places(&buffers),
                _ => Vec::new(),
            };
            state.kept = buffers;
            (untaken, frame)
        };
        match untaken {
            Ok(None) => Ok(()),
            Ok(Some(outcomes)) => {
                answer(py, &self.places, &self.named, self.loss, &outcomes, frame)
            }
            Err(e) => Err(raised(py, &self.places, &e)),
        }
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

    /// The one place that is not an index, for a value with no key.
    fn only(&self) -> PyResult<usize> {
        self.only.ok_or_else(|| {
            let count = (0..self.places.len())
                .filter(|&place| !indexed(&self.places, place))
                .count();
            PyTypeError::new_err(format!(
                "the writer has {count} places that are not an index: pass a dict from \
                 each channel name to its value"
            ))
        })
    }
}

#[pymethods]
impl Writer {
    /// The name, the type, and the place of the index of each place, in place order,
    /// the type as the spec writes it (`f64`, `timestamp`). The place of an index is
    /// its first place, also at a repeat of it. Place `n` is channel `n` of the open;
    /// then comes the index of each group that the open did not name, in the order of
    /// the first channel of each group.
    #[getter]
    fn places<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        crate::places(py, &self.places)
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
    /// when the writer has one place that is not an index. A value at an index is the
    /// stamp of its group, in nanoseconds since the UNIX epoch. The values give the
    /// stamp of each group of the frame or of none; with none, the node stamps each
    /// group. A value is a `bool` or a `numpy.bool_` for `bool`, an `int`
    /// (or an object with `__index__`) for an integer type and for `timestamp` and
    /// `duration` (nanoseconds), a `float` (or an object with `__float__` or
    /// `__index__`, such as an `int`) for `f32` and `f64`, and a `uuid.UUID` for
    /// `uuid`. The call reads `values` after it waits for a call of another thread that
    /// sends. A change of `values` after that read is not in the frame. Other Python
    /// threads run while it waits.
    ///
    /// Raises `TypeError` for a value that is not of its place's type, a place of a
    /// type that is not a scalar, a key that is not a name or a place, or one value
    /// with no key for a writer of more places; `OverflowError` for a number outside
    /// its type; `KeyError` for a name of no place; `ValueError` for no value, a place
    /// past the places, two values of one channel, or the stamps of only some groups of
    /// the frame; and each error other than `TypeError` and `OverflowError` that the
    /// `__float__` or `__index__` of a value raised. Each `TypeError` of the conversion
    /// of a value is the `__cause__` of the `TypeError` that names its place. With each
    /// of these, nothing was sent and the writer goes on. When the node did not take
    /// each group of the frame, raises for the first that holds: a group whose write
    /// ended raises `GroupEnded`, also in a writer of one index; a refused group raises
    /// `Rejected`; and a lost group raises `DataLost` with `on_loss="raise"`, with the
    /// channels of the lost groups only. The text of each names the cause of each group
    /// that the node did not take. Raises `Stopped` when the node stopped the stream,
    /// so the writer has ended, and `ConnectionError` when the session failed, after
    /// each of which each call raises it again; `RuntimeError` after a close or in a
    /// forked process; and `PanicException` when the client's thread panicked.
    fn write(&self, py: Python<'_>, values: &Bound<'_, PyAny>) -> PyResult<()> {
        #[cfg(feature = "sim")]
        let began = crate::sim::Part::start();
        let buffers = self.fill(py, |buffers| {
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
            untaken(writer.write(&buffers.values))
        };
        self.send(py, buffers, send, |buffers| {
            buffers.values.iter().map(|&(place, _)| place).collect()
        })
    }

    /// Writes one frame of each series of `series`, a `dict` from a channel name or a
    /// place to a one-dimensional `numpy.ndarray`, and blocks until the node took it.
    /// The arrays of one group have one length, and the series of each index holds the
    /// stamps of its group.
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
    /// of a type that is not a scalar, and `ValueError` for arrays of one group with
    /// different lengths, an array of more than 4294967295 samples, series over the
    /// bytes of one frame (before their copy), no series, a group with series and no
    /// series of its index, a frame over the largest block, or an array that another
    /// call writes to.
    /// With each of these, nothing was sent and the writer goes on. Raises each other
    /// error as `write` raises it.
    fn write_series(&self, py: Python<'_>, series: &Bound<'_, PyDict>) -> PyResult<()> {
        #[cfg(feature = "sim")]
        let began = crate::sim::Part::start();
        let buffers = self.fill(py, |buffers| {
            buffers.bytes.clear();
            buffers.series.clear();
            // A copy, as on Python 3.11 an allocation in the loop, such as the
            // arguments of the `view` of a `bool` array, can run the garbage collector
            // and so a `__del__` that changes the dict.
            for (key, array) in series.copy()?.iter() {
                let place = self.place(&key)?;
                let start = buffers.bytes.len();
                let (bytes, max) = (&mut buffers.bytes, self.bytes_max);
                let count = copy(&self.places[place], &array, bytes, max)?;
                buffers
                    .series
                    .push((place, start..buffers.bytes.len(), count));
            }
            Ok(())
        })?;
        let send = move |writer: &mut sdk::write::Writer, buffers: &Buffers| {
            let Buffers { bytes, series, .. } = buffers;
            let parts: Vec<_> = series
                .iter()
                .map(|(place, range, count)| sdk::write::Series {
                    place: *place,
                    count: *count,
                    bytes: &bytes[range.clone()],
                })
                .collect();
            #[cfg(feature = "sim")]
            self.part.mark(began);
            untaken(writer.write_series(&parts))
        };
        self.send(py, buffers, send, |buffers| {
            buffers.series.iter().map(|&(place, ..)| place).collect()
        })
    }

    /// Finishes the stream, and blocks until each frame that the node took is on disk,
    /// or its group ended, and the node finished the stream. Control ends then. A close
    /// waits for a call that sends, with other Python threads running. A call that a
    /// close ends before its send raises `RuntimeError`. A close after a close gives
    /// `None`, and in a forked process it gives `None` and ends nothing.
    ///
    /// Raises `Stopped` when the node stopped the stream, or when the write of a group
    /// ended with a frame that the node took and that may not be on disk, with `code`
    /// the end of the first such group and a text that names the index and the end of
    /// each. Raises `ConnectionError` when the session failed, `RuntimeError` when the
    /// client closed, and `PanicException` when the client's thread panicked.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if process::id() != self.process {
            return Ok(());
        }
        let state = self.lock(py).take();
        state.map_or(Ok(()), |state| {
            py.detach(|| state.writer.close())
                .map_err(|e| raised(py, &self.places, &e))
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

/// What a call of the `sdk` writer gave: `None` when the node took each group of the
/// frame, else the place of the index and the outcome of each group, in the order of
/// their index places.
type Untaken = Result<Option<Vec<(usize, Outcome)>>, sdk::Error>;

/// The [`Untaken`] of `written`, what a call of the `sdk` writer gave.
fn untaken(written: Result<&[(usize, Outcome)], sdk::Error>) -> Untaken {
    match written {
        Ok(outcomes)
            if outcomes
                .iter()
                .all(|(_, outcome)| matches!(outcome, Outcome::Taken { .. })) =>
        {
            Ok(None)
        }
        Ok(outcomes) => Ok(Some(outcomes.to_vec())),
        Err(sdk::Error::Partial { outcomes, .. }) => Ok(Some(outcomes)),
        Err(error) => Err(error),
    }
}

/// What the text of a loss in a frame of a writer names.
const LOST: &str = "samples were lost";

/// Answers a frame whose groups did not all get `Taken`, by the first rule that holds:
/// a group whose write ended raises `GroupEnded`; a refused group raises `Rejected`;
/// and a lost group does what `loss` says, with the channels of the lost groups, so it
/// gives `Ok` for `warn` and `ignore`. `GroupEnded` and `Rejected` give the seqs of the
/// lost groups in `lost`, and with `warn` also log the loss. `outcomes` is the place of
/// the index and the outcome of each group, in the order of their index places, and
/// `frame` the places of the items of the call, of the writer of `places` and `named`.
/// The channels of the frame are those of `frame` and the index of each group, each
/// named once.
pub(crate) fn answer(
    py: Python<'_>,
    places: &[Place],
    named: &hash::Map<String, usize>,
    loss: Loss,
    outcomes: &[(usize, Outcome)],
    frame: Vec<usize>,
) -> PyResult<()> {
    let frame = channels(places, named, outcomes, frame);
    let end = outcomes.iter().find_map(|&(_, outcome)| match outcome {
        Outcome::Ended(end) => Some(end),
        _ => None,
    });
    let refused = outcomes.iter().find_map(|&(_, outcome)| match outcome {
        Outcome::Refused(refusal) => Some(refusal),
        _ => None,
    });
    let split = || {
        let (mut written, mut unwritten, mut missed) =
            (Vec::new(), Vec::new(), Vec::new());
        for &place in &frame {
            let name = places[place].name.as_str();
            match outcome(places, outcomes, place) {
                Outcome::Taken { .. } => written.push(name),
                Outcome::Refused(_) => {
                    unwritten.push(name);
                    missed.push(name);
                }
                Outcome::Lost { .. } | Outcome::Ended(_) => missed.push(name),
            }
        }
        let text = causes(places, outcomes, &written, &missed);
        (written, unwritten, text)
    };
    let gone = || lost(places, outcomes, &frame);
    let listed = |raised: PyErr, written: Vec<&str>, unwritten: Vec<&str>| {
        let value = raised.value(py);
        value.setattr("written", PyTuple::new(py, written)?)?;
        value.setattr("unwritten", PyTuple::new(py, unwritten)?)?;
        value.setattr("lost", crate::seqs(py, gone())?)?;
        if loss == Loss::Warn && gone().next().is_some() {
            loss.act(py, LOST, gone(), py.None().bind(py))?;
        }
        Err(raised)
    };
    match (end, refused) {
        (Some(end), _) => {
            let (written, unwritten, text) = split();
            let text = format!(
                "{text}; each later frame of an ended group raises again, and the \
                 other groups go on"
            );
            let raised = crate::coded(py, GroupEnded::new_err(text), stop(end));
            let ended = PyTuple::new(py, ended(places, named, outcomes))?;
            raised.value(py).setattr("ended", ended)?;
            listed(raised, written, unwritten)
        }
        (None, Some(refusal)) => {
            let (written, unwritten, text) = split();
            let raised = Rejected::new_err(text);
            raised.value(py).setattr("code", rejection(refusal).0)?;
            listed(raised, written, unwritten)
        }
        (None, None) => loss.act(py, LOST, gone(), py.None().bind(py)),
    }
}

/// The outcome of the group of the channel at `place` of a writer of `places`, by
/// `outcomes`.
fn outcome(places: &[Place], outcomes: &[(usize, Outcome)], place: usize) -> Outcome {
    let index = places[place].index;
    let at = outcomes
        .binary_search_by_key(&index, |&(index, _)| index)
        .expect("invariant: each group of the frame has its outcome");
    outcomes[at].1
}

/// The name of each channel at `frame` of a writer of `places` in a lost group, by
/// `outcomes`, with the seqs of its lost samples.
fn lost<'a>(
    places: &'a [Place],
    outcomes: &'a [(usize, Outcome)],
    frame: &'a [usize],
) -> impl Iterator<Item = (&'a str, Range<u128>)> + Clone {
    frame
        .iter()
        .filter_map(move |&place| match outcome(places, outcomes, place) {
            Outcome::Lost { range } => {
                let start = u128::from(range.seq);
                let seqs = start..start + u128::from(range.count);
                Some((places[place].name.as_str(), seqs))
            }
            _ => None,
        })
}

/// The place of each channel name of `places`: the first place of a name that two
/// places hold.
pub(crate) fn named(places: &[Place]) -> hash::Map<String, usize> {
    let mut named = hash::Map::default();
    for (place, Place { name, .. }) in places.iter().enumerate() {
        named.entry(name.as_str().to_owned()).or_insert(place);
    }
    named
}

/// The name of each channel of the writer of `places` and `named` in a group whose
/// write ended, by `outcomes`, once each, in place order.
fn ended<'a>(
    places: &'a [Place],
    named: &hash::Map<String, usize>,
    outcomes: &[(usize, Outcome)],
) -> Vec<&'a str> {
    let ended = |index: usize| {
        outcomes
            .binary_search_by_key(&index, |&(index, _)| index)
            .is_ok_and(|at| matches!(outcomes[at].1, Outcome::Ended(_)))
    };
    places
        .iter()
        .enumerate()
        .filter(|&(at, place)| ended(place.index) && named[place.name.as_str()] == at)
        .map(|(_, place)| place.name.as_str())
        .collect()
}

/// The first place of each channel of a frame of the items at `frame` of a writer of
/// `places` and `named` and of the index of each group of `outcomes`, once each, in
/// place order.
fn channels(
    places: &[Place],
    named: &hash::Map<String, usize>,
    outcomes: &[(usize, Outcome)],
    mut frame: Vec<usize>,
) -> Vec<usize> {
    frame.extend(outcomes.iter().map(|&(index, _)| index));
    for place in &mut frame {
        *place = named[places[*place].name.as_str()];
    }
    frame.sort_unstable();
    frame.dedup();
    frame
}

/// The cause of each group of `outcomes` that the node did not take, by the name of its
/// index, then the channels of the frame that it did not write, `missed`, and those
/// that it wrote, `written`.
fn causes(
    places: &[Place],
    outcomes: &[(usize, Outcome)],
    written: &[&str],
    missed: &[&str],
) -> String {
    let mut causes = Vec::new();
    for &(index, outcome) in outcomes {
        let index = &places[index].name;
        causes.push(match outcome {
            Outcome::Taken { .. } => continue,
            Outcome::Ended(end) => {
                let code = crate::code(stop(end));
                format!("the write of {index} ended ({code}: {})", why(end))
            }
            Outcome::Refused(refusal) => {
                let (code, why) = rejection(refusal);
                format!("the node refused the frame of {index} ({code}: {why})")
            }
            Outcome::Lost { range } => {
                format!("the frame of {index} lost {} samples", range.count)
            }
        });
    }
    let written = if written.is_empty() {
        "no channel was".to_owned()
    } else {
        format!("channels {} were", written.join(", "))
    };
    format!(
        "{}; channels {} were not written; {written}",
        causes.join("; "),
        missed.join(", ")
    )
}

/// Why the write of a group ended with `end`.
fn why(end: End) -> &'static str {
    match end {
        End::Removed => "a channel of its group was removed",
        End::Failed => "the home of its group failed",
        End::Refused => "the home of its group refused the proof of the program",
    }
}

/// The exception of `error`, which a call or the close of a writer of `places` gave.
/// Its text names the channels at the places of `error`: for `Unstored`, the index and
/// the end of each group, with the code of the first; for a refusal of the stream, it
/// adds that the writer has ended.
pub(crate) fn raised(py: Python<'_>, places: &[Place], error: &sdk::Error) -> PyErr {
    let name = |place: usize| places[place].name.as_str();
    let text = match error {
        sdk::Error::Stream(program::Error::Refused(_)) => {
            format!("{error}; the writer has ended")
        }
        sdk::Error::Stream(program::Error::Unstored(groups)) => {
            let named: Vec<_> = groups
                .iter()
                .map(|&(index, end)| {
                    format!("{} ({})", name(index), crate::code(stop(end)))
                })
                .collect();
            let writes = if named.len() == 1 { "write" } else { "writes" };
            let text = format!(
                "the {writes} of {} ended: a frame that the node took may not be on \
                 disk",
                named.join(", ")
            );
            let (_, end) = groups.first().expect("invariant: Unstored names a group");
            return crate::coded(py, Stopped::new_err(text), stop(*end));
        }
        sdk::Error::Stream(program::Error::Count {
            place,
            count,
            other,
            other_count,
        }) => format!(
            "channels {} and {} are of one index but hold {count} and {other_count} \
             samples",
            name(*place),
            name(*other)
        ),
        sdk::Error::Stream(program::Error::Layout(frame::Error::IndexAbsent {
            entry,
            index,
        })) => format!(
            "the frame holds {} but not its index, {}",
            name(*entry),
            name(*index)
        ),
        sdk::Error::Stream(program::Error::Layout(frame::Error::Unordered {
            entry,
            ..
        })) => format!("the frame holds {} twice", name(*entry)),
        _ => return crate::error(py, error),
    };
    crate::exception(py, error, text)
}

/// Whether place `place` of `places` is an index.
fn indexed(places: &[Place], place: usize) -> bool {
    places[places[place].index].key == places[place].key
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
                Err(e) => return Err(failed(place, object, e)?),
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
        Scalar::Uuid => match crate::uuid_sample(object)? {
            Some(sample) => Value::Uuid(sample),
            None => return Err(wrong(place, object)?),
        },
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
        Err(e) => return Err(failed(place, object, e)?),
    };
    T::try_from(wide)
        .map_err(|_outside| outside(place, object, &format!("{min} to {max}")))
}

/// The error of `object`, whose conversion for `place` raised `e`: the `TypeError` of a
/// value of another type, with `e` as its cause, when `e` is a `TypeError`, or else `e`.
fn failed(place: &Place, object: &Bound<'_, PyAny>, e: PyErr) -> PyResult<PyErr> {
    let py = object.py();
    if e.is_instance_of::<PyTypeError>(py) {
        let error = wrong(place, object)?;
        error.set_cause(py, Some(e));
        return Ok(error);
    }
    Ok(e)
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
    use sdk::Place;
    use types::channel;
    use types::sample::{Scalar, Type, Value};

    /// A place of `scalar`.
    fn place(scalar: Scalar) -> Place {
        Place {
            key: channel::Key::from_u128(1),
            data_type: Type::Scalar(scalar),
            name: "plant.a".parse().unwrap(),
            index: 0,
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
