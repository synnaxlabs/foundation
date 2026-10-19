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
  The numbers below come from `sdk/python/bench/read.py` at 9cea9ca19, with the
  `wheel` profile and the feature `sim` (the reader code of 8bb532882), and a release
  node, on box1 (Xeon Platinum 8488C, 32 CPUs, load 13 to 34 by `/proc/loadavg`), in
  µs. The reader times the part of `__next__` after `next_within` gives the frame
  (`Reader._part`): the copy of each series, the arrays, the `dict`, and the `Frame`.
  Each case runs on a new node, with 64 MiB of disk per core, and times 3000 frames
  after 50 to warm up, or the frames that fit in 48 MiB of series (in brackets). Each
  cell gives the range over 5 passes. Places are of `f64` and do not count the index,
  whose stamps each frame also holds. "As it comes" reads each frame after its write;
  "queued" reads the frames after all their writes, with no wait.

  | Case                      | As it comes, p50 | p99       | Queued, p50 | p99       |
  | ------------------------- | ---------------- | --------- | ----------- | --------- |
  | 1 place, 100 samples      | 1.95-15.9        | 9.49-36.1 | 0.90-1.02   | 1.27-3.07 |
  | 1 place, 1000 samples     | 4.85-8.49        | 16.3-24.1 | 2.16-2.36   | 2.98-7.60 |
  | 1 place, 3000 (998)       | 5.49-15.5        | 21.6-30.3 | 5.01-10.3   | 13.5-24.3 |
  | 16 places, 100 samples    | 7.38-21.2        | 21.9-43.1 | 4.24-6.20   | 8.24-16.0 |
  | 64 places, 100 (917)      | 23.5-54.9        | 55.2-84.8 | 14.0-28.3   | 30.3-50.4 |

  One place of `bool`, as it comes, p50 then p99: 1.90-8.67, 8.19-22.3 at 100 samples;
  2.78-12.5, 12.8-25.3 at 1000; and 4.25-16.2, 16.1-34.0 at 3000 (1814). The other cases
  are frames over the largest block of the pool of frames of a writer (57344 bytes),
  which the writer of the bench cannot send. The round trip of a `write_series` of one
  place to the `__next__` of its frame, p50 of 400 frames: 6.6 to 10.1 ms in 4 passes,
  and 22.9 to 35.0 ms in the pass whose load rose to 34. A complete reader gets a frame
  only after it is on disk (B1), so the group commit and its sync set the round trip. A
  latest reader, in one run of 200 frames of 100 samples, got each frame in 69 µs at
  p50.

  P1 judgment. The part costs about 0.3 to 0.4 µs for each array (the `Vec`, the `numpy`
  array, and its item in the `dict`), from the queued cases of 1, 16, and 64 places, and
  about 0.7 ns for each sample that it copies. A frame as it comes costs 1 to 15 µs more
  at p50 than a queued one: the thread of the program wakes after it slept through the
  commit of about 7 ms, with cold caches. It runs once for each frame that a Python
  program reads, on the CPU of the program, not of the node, and adds 0 bytes per
  sample. It is on the path of a latest-mode read, so it counts against the latest-mode
  p99, whose limit is 2.5 µs a hop. Its p99 is over the limit in each case but the
  queued frame of 1 place of 100 samples, at 1.27 to 3.07 µs. A cut of the part per
  sample needs arrays that view the buffers of the `sdk` frame, which this item rejects
  above. A cut of the part per array needs no `numpy` array for each series, which the
  Python surface cannot give.
