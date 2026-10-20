- **SDK PYTHON READER** `Client.open_reader(channels, *, mode="complete",
  on_loss="warn")` of `sdk-python` opens a `foundation.Reader` over
  `sdk::read::Reader`, with the place order of `sdk`. `for frame in reader:` gives each
  frame as a `foundation.Frame`, a read-only `collections.abc.Mapping` from the name
  of each channel of the frame, the index included, to a new one-dimensional
  `numpy.ndarray` that the program owns, with `Frame.count` and `Frame.lost`. A channel
  of `uuid` reads as an array of dtype `object`, with one `uuid.UUID` for each sample,
  made after the guard from the 16 bytes of the sample, big-endian, which costs one
  Python object a sample (below). `laptop.architect-2` approved it at
  2026-10-19T22:22:38Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6278668296). A
  series of `uuid` to `Writer.write_series`, and one `mod uuid` that holds the class
  of `uuid.UUID` once for the reader and the writer, wait on #3464. The overloads of
  `Frame.get` in the stub wait on #3481. `laptop.architect-2` deferred both at
  2026-10-20T01:33:55Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6281242356). Only a
  channel of a type that is not a scalar raises `TypeError` at the open. The reader
  follows the writer (SDK PYTHON WRITER): one `Mutex` taken with
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
     `timeout`, and the call stays out, so no frame is lost. A drop of the reader while
     its call is out ends its stream. `__next__` waits in steps of 100 ms with the GIL
     released, and runs the signal handlers between them, and none while a `__next__`
     of another thread holds the reader, so a program reads a reader on one thread. Its
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
  wakes the client's thread each 100 ms for each waiting reader; `mode` as a class,
  where a `str` matches `on_loss`; and each array made at the first read of its name,
  which cuts the part only for a program that reads fewer channels than it opened, and
  adds a cache to `Frame`.
  The numbers below come from `sdk/python/bench/read.py` at 13aa18851, with the `wheel`
  profile and the feature `sim` (the reader code of 6c996bc1e), and a release node, on
  box1 (Xeon Platinum 8488C, 32 CPUs, load 9.7 to 19.7 by `/proc/loadavg`), in µs. The
  reader times the part of `__next__` after `next_within` gives the frame
  (`Reader._part`): the copy of each series, the arrays, the `dict`, and the `Frame`.
  Each case runs on a new node, with 64 MiB of disk per core, and times 3000 frames
  after 50 to warm up, or the frames that fit in 48 MiB with 48 bytes for each series
  (in brackets). Each cell gives the range over 5 passes. Places are of `f64` and do not
  count the index, whose stamps each frame also holds. A case is places x samples.
  "Comes" (as it comes) reads each frame after its write; "queued" reads the frames
  after all their writes, with no wait; "whole" is the whole call of `__next__` of a
  queued frame and the free of the frame, p50 then p99.

  | Case         | Comes p50 | p99       | Queued p50 | p99       | Whole   | p99     |
  | ------------ | --------- | --------- | ---------- | --------- | ------- | ------- |
  | 1x100        | 2.20-7.85 | 8.49-16.5 | 0.68-0.94  | 1.41-3.58 | 10-18   | 28-38   |
  | 1x1000       | 3.96-8.70 | 14.5-25.6 | 2.14-2.51  | 3.08-9.67 | 19-23   | 161-266 |
  | 1x3000 (996) | 8.78-13.4 | 22.3-29.2 | 5.13-12.5  | 11.4-22.6 | 27-41   | 135-459 |
  | 16x100       | 10.5-14.0 | 22.6-32.4 | 3.12-5.64  | 5.21-10.1 | 15-28   | 106-280 |
  | 64x100 (863) | 24.0-28.5 | 42.5-57.1 | 15.5-22.4  | 36.5-45.3 | 62-72   | 266-683 |
  | 1024x1 (826) | 159-242   | 245-293   | 133-229    | 182-278   | 327-531 | 571-921 |

  One place of `bool`, as it comes, p50 then p99: 2.78-7.14, 10.0-17.6 at 100 samples;
  2.64-8.08, 11.2-23.7 at 1000; and 4.09-9.83, 9.27-24.1 at 3000 (1807). A queued frame
  of one sample of `uuid`, in 5 more passes at load 14.3 to 15.5: p50 1.74-2.75, p99
  4.71-7.42, against p50 0.55-0.79, p99 1.16-2.65 for one sample of `f64`. The other
  cases are frames over the largest block of the pool of frames of a writer (57344
  bytes), which the writer of the bench cannot send. The round trip of a `write_series`
  of one place of 100, 1000, or 3000 samples to the `__next__` of its frame, p50 of 400
  frames: 6.57 to 8.15 ms for a complete reader, which gets a frame only after it is on
  disk (B1); and for a latest reader, 71.1 to 84.3 µs at 100 samples in 4 passes and 791
  µs in the fifth, 122.8 to 841.9 µs at 1000, and 245.5 to 969.9 µs at 3000.

  P1 judgment. The part costs about 0.13 to 0.35 µs for each array (the `Vec`, the
  `numpy` array, and its item in the `dict`), from the queued p50 of 64 and 1024 places
  (65 and 1025 arrays with the index) against 1 place, and 0.65 to 2.6 ns for each
  sample that it copies, from the queued p50 of 1 place at 100, 1000, and 3000 samples
  (two arrays a frame). In each pass, a queued frame of one sample of `uuid` costs 1.2
  to 2.1 µs more at p50 than one of `f64`. The part alone reaches 250 µs at p99 at about
  1024 places (182 to 293 µs). In each pass, a frame as it comes costs from 2.3 µs less
  to 12.3 µs more at p50 than a queued one, at up to 64 places. The whole call adds 9.4
  to 49.2 µs at p50 to the part of a queued frame at up to 64 places, and 194 to 387 µs
  at 1024: the wait of `next_within`, the release and take of the GIL, one `getpid`, and
  the free of the frame. It runs once for each frame that a Python program reads, on the
  CPU of the program, not of the node, and adds 0 bytes per sample. It is on the path of
  a latest-mode read, so it counts against the latest-mode p99, whose limit is 2.5 µs a
  hop. Its p99 is over the limit in each case of the table, and in at least one pass of
  the queued frame of 1 place of 100 samples (1.41 to 3.58 µs) and of one sample of
  `f64` (1.16 to 2.65 µs). A queued frame of one sample of `uuid` is over it at p99 in
  each pass (4.71 to 7.42 µs), and its cost grows with each sample: each costs one
  `uuid.UUID(int=...)`, 0.50 to 1.09 µs by `timeit` in Python 3.12.3 on box1 at load 18.
  A `UUID` made with `object.__new__` and its fields `int` and `is_safe` costs 0.16 µs,
  but it depends on a layout of `uuid.UUID` that Python does not state. A cut of the
  part per sample needs arrays that view the buffers of the `sdk` frame, which this item
  rejects above. A cut of the part per array needs no `numpy` array for each series,
  which the Python surface cannot give.
  `laptop.architect-2` accepted this judgment at 0804db16e at 2026-10-19T21:54:00Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6278228544), with the
  line of 1024 places as its trigger at 2026-10-19T22:22:38Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6278668296), held
  it on this table at 2026-10-19T23:20:54Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6279518607), and held
  it, with the cost of `uuid` for each sample, at 485e0e045 at 2026-10-20T01:23:16Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6281113462).
  In the quiet-host rerun of `benches/sdk` of the node, base ecc627b57 against head
  c4a0ec1d2, 3 runs each, interleaved, on a c7i.metal-24xl pinned to CPU 2
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6281922246),
  "sdk next held" moves +0.6 µs at p99, and "sdk read" +0.3 µs at p50 and +0.9 µs at
  p99, while "program read", which runs no code of the reader, moves +4.0 µs at p99.
  On these numbers `laptop.architect-2` ruled that P1 holds the merge no more, at
  2026-10-20T02:52:22Z
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6282212756), and,
  at 2026-10-20T02:41:58Z, that its acceptance covers the `read.py` line of one place
  of 100 samples with no rerun
  (https://github.com/synnaxlabs/foundation/pull/3436#issuecomment-6282088053).
