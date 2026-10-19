- **SDK PYTHON READER** `Client.open_reader(channels, *, mode="complete",
  on_loss="warn")` of `sdk-python` opens a `foundation.Reader` over
  `sdk::read::Reader`, with the place order of `sdk`. `for frame in reader:` gives each
  frame as a `foundation.Frame`, a read-only `collections.abc.Mapping` from the name
  of each channel of the frame, the index included, to a new one-dimensional
  `numpy.ndarray` that the program owns, with `Frame.count` and `Frame.lost`. A
  channel of `uuid`, or of a type that is not a scalar, raises `TypeError` at the open,
  as `numpy` has no dtype of 16-byte samples that the writer takes. The reader follows
  the writer (SDK PYTHON WRITER): one `Mutex` taken with
  `MutexExt::lock_py_attached`, a check of the process before the lock, and no Python
  code while a call holds the lock. The plan
  (https://github.com/synnaxlabs/foundation/issues/3321#issuecomment-6272864204) was
  approved by `laptop.architect-2` at 2026-10-19T18:17:39Z
  (https://github.com/synnaxlabs/foundation/issues/3321#issuecomment-6274120136),
  with the ask of `laptop.monitor` that the frame is the mapping, with no
  `Frame.series`, and with these terms:
  1. No Python object is made or dropped while the guard of the reader lives, as
     Python code that then runs (a finalizer of the cycle collector of Python 3.11, or
     a `__del__`) can call the reader and wait on its own lock for good. Under the
     lock, `__next__` copies each series into a `Vec` of its type, and drops the guard.
     Then `PyArray1::from_vec` takes each `Vec` with no second copy. A `bool` series
     maps each byte with `!= 0`. The signal check and the warning of `"warn"` also run
     with no guard.
  2. A `Pool` error of the stream of a reader raises `ConnectionError`: only
     `TooLarge` gets there, a frame of the node over the read budget of the client,
     which ends the reader and is not a bad argument.
  3. `DataLost.frame` is the frame after the loss when a reader raises it, and `None`
     when a writer raises it.
  4. `sdk::read::Reader::next_within(timeout)` gives `None` when no frame came within
     `timeout`, and the call stays out, so no frame is lost. `__next__` waits in steps
     of 100 ms with the GIL released, and runs the signal handlers between them. Its
     wait reads the monotonic clock of the OS on the caller's thread, only as the bound
     of a wait, never as a time of data.
  A close sets a flag before it takes the lock, and `__next__` reads the flag before
  each step, because a `std::sync::Mutex` lets the thread that loops in `__next__`
  take the lock again before a close that waits on it. So a close on another thread
  ends a `__next__` that waits within one step.
  Lost: arrays that view the buffers of the `sdk` frame, valid until the next
  `__next__`, as a program that keeps an array then gets an error or pays the copy
  anyway; a `Frame::take` in `sdk`, a new public item whose `Vec<u8>` does not have the
  alignment of an `f64` array; a timer in the task of the client for the bound, which
  wakes the client's thread each 100 ms for each waiting reader; and `mode` as a class,
  where a `str` matches `on_loss`.
