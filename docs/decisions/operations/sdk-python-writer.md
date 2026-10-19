- **SDK PYTHON WRITER** `Client.open_writer(channels, *, authority=0, lease=None,
  on_loss="warn")` of `sdk-python` opens a `foundation.Writer` over `sdk::write::Writer`
  (SDK WRITER), with the place order of `sdk`. `Writer.write` takes a `dict` from a
  channel name or a place to one value, or one value when the writer has one place
  other than the index. `Writer.write_series` takes a `dict` to one-dimensional
  `numpy.ndarray`s of the exact dtype of each place, in native byte order, and casts
  nothing. A stamp is an `int` of nanoseconds since the UNIX epoch, and
  `write_series` also takes `datetime64[ns]` for `timestamp` and `timedelta64[ns]` for
  `duration`. The binding checks each value and array against its place, then calls
  `sdk` with the GIL released. A writer of one process raises `RuntimeError` in a child
  that the process forks, and its close there gives `None` and ends nothing. The plan
  (https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257542875) was
  approved by `laptop.architect-2` at 2026-10-19T01:19:25Z
  (https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257620825), with
  these terms:
  1. The error map. `sdk::Error::Session` with `program::Error::Refused` raises
     `Refused`, a subclass of `ConnectionError`: the session ended, so connect again.
     `Stream` with `Refused` raises `Stopped`, a subclass of `Exception`: at the open,
     no writer opened and the session goes on, and a writer that raises it has ended.
     Each has the lower-case `code` of the refusal, and its docstring names the codes
     that it gives. The text of a `Stopped` with `unknown` from `open_writer` names
     each channel of the open, as the refusal holds only a code. Changed in round 1 of
     PR 3325 by `laptop.architect-2` at 2026-10-19T02:21:46Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6258290968), with
     the OK of `laptop.monitor` at 2026-10-19T02:22:22Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6258297079):
     before, `Refused` came for both, so `except ConnectionError` took a typo in a
     channel name for a lost connection. Lost: one `Refused(Exception)` for both,
     because a caller of `connect` that catches `ConnectionError` then misses a
     refused session; and a `session` attribute on `Refused`, because
     `except ConnectionError` then still catches a typo. `Stream` with `Layout`,
     `Unsent`, `Pool`, `Body`, `Open`, or `Unnamed` raises `ValueError`: nothing was
     sent, and the writer goes on, or, at the open, no writer opened. Each other
     `Stream` or `Session` error raises `ConnectionError`.
  2. `sdk-python` takes `program` as a normal dependency and matches
     `program::Error::Refused` itself. Lost: a re-export of `program::Error` and
     `program::Refusal` from `sdk`, two public items that no Rust caller names yet. The
     later step `sdk::Refusal` of #2177 replaces both when one does. The edge crosses
     the two lists. `laptop.architect` gave its OK at 2026-10-19T01:20:17Z
     (https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257630032):
     the binding names only the error types of `program`, never a `Session`,
     `Reader`, or `Writer`.
  3. After `DataLost`, the writer goes on, because a later frame can find room.
  4. One `Mutex` holds the `sdk` writer and the kept buffers, so no call takes two
     locks. Each call checks the process before the lock, because a lock that another
     thread held at a fork stays held in the child, and then takes the lock with
     `MutexExt::lock_py_attached`, which waits with the GIL released. So a second
     Python thread that writes on the same writer cannot hold the GIL while the first
     waits for it. No Python code runs while a call holds the lock: a call takes the
     kept buffers under the lock and fills them with no lock held, takes the lock again
     for the `sdk` call and to keep the buffers, and raises or logs after it lets the
     lock go. So a value (`__float__`, `__index__`), a logging handler, or a `__del__`
     can call the same writer. A call that finds the buffers taken fills new ones, and
     a call that raises in the fill gives the buffers back. A close waits for a call
     that sends, not for one that fills: a call that a close ends before its send
     raises `RuntimeError`. Both came from round 2 items 5 and 6 of PR 3325, at
     2026-10-19T12:35:34Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267342407).
     The call of `outcome` after the lock goes was changed in round 1 of PR 3325 by
     `laptop.architect-2` at 2026-10-19T02:21:46Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6258290968):
     before, a call held the lock while it logged. The fill with no lock came from
     round 1 item 2 of PR 3325, at 2026-10-19T03:27:44Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6259002324):
     before, a call held the lock while it converted values, so a value that called
     the writer hung. `laptop.architect-2` ruled it OK at 2026-10-19T12:46:15Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267540431).
     Lost: a second lock for the buffers, since a call then takes two
     locks; and a re-entrant lock, since an inner call then changes the buffers that
     the outer call fills.
  5. `DataLost.seqs` is a `dict` from each channel name of the lost frame, the index
     included, to the `range` of its lost seqs, the shape that the reader of PR 7b
     gives. The text names each channel and its count.
  6. `Stopped` and `Rejected` are two classes, as their code sets differ. A writer has
     ended after `Stopped`, and each later call raises it again. It goes on after
     `Rejected`, the refusal of one frame (`waiting`, `reserved`, `expired`, `order`,
     `codec`, `full`, or `large`). Changed with item 1 (same links): `Stopped` was
     `Refused`.
  7. `write_series` copies each array into the kept buffer with the GIL held, because
     another Python thread can change an array while the GIL is free. Before the copy
     of each array, it raises `ValueError` when the bytes copied so far and the bytes
     of the array go over `sdk::write::Writer::bytes_max`, which the writer reads at
     the open, so no copy goes past one frame. Decided by `laptop.architect-2` at
     2026-10-19T12:51:02Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267629281), for
     the round 3 defect of PR 3325: before, a broadcast array of 2^31 samples ended the
     process in its copy. The copy reads an array that is not aligned or not
     contiguous in place, at its byte stride, with one `unsafe` read of each sample.
     Changed by `laptop.architect-2` at 2026-10-19T13:03:37Z
     (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267870899):
     before, `numpy.array` copied such an array first, and `numpy` lets other threads
     run in that copy. Its cost is measured below, with the P1 judgment.
  The key choices of the plan went ahead with `laptop.monitor` at 2026-10-19T01:13:34Z
  (https://github.com/synnaxlabs/foundation/issues/3320#issuecomment-6257555546), with
  one condition: the `TypeError` of an array of another dtype names the place, the
  dtype that it needs, the dtype given, and the fix, such as `plant.a holds f64 and
  takes an array of float64, not int64: pass a.astype("float64")`. The wheel requires
  the Python package `numpy>=1.26`.
  Lost: a split of `sdk::Writer::write_series` into a fill with the GIL held and a
  send with it released, a new public item of `sdk` for one copy fewer.
  The limit of 2.5 µs p99 applies only to the part of a call from the Python call to
  the call of `sdk::write::Writer::write`, which holds the name lookup, the
  conversion, and the copy of `write_series` (ruling of the director,
  2026-10-19T02:28:28Z,
  https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6258363742). The
  numbers below come from `sdk/python/bench/write.py`, with the `wheel` profile and
  the feature `sim`, and a release node, on box2 (Xeon Platinum 8488C, 32 CPUs, load
  2 to 6 by `uptime`) at 3ab8c7246, in µs. The writer times that part itself
  (`Writer._part`), from the start of a call to just before the `sdk` call, so the
  times also hold both locks, the copy of the `dict`, the release of the GIL, and,
  for `write_series`, the `Vec` of its parts. It times 3000 calls with warm buffers,
  after 50 calls to warm up. Each cell gives the range over 5 passes, each on a new
  node. Places are of `f64` and do not count the index, whose stamps each
  `write_series` also copies. The p99:

  | Places | `write`   | `write_series` 100 | 1000       | 3000       |
  | ------ | --------- | ------------------ | ---------- | ---------- |
  | 1      | 0.75-4.88 | 1.07-6.03          | 5.74-7.46  | 7.09-15.0  |
  | 16     | 2.67-6.15 | 8.50-9.68          | 7.93-11.6* | 23.9-32.3* |
  | 64     | 6.15-9.96 | 28.9-33.2          | 35.5-70.6* | 126-143*   |
  | 255    | 35.2-41.0 | 39.5-81.1*         | 184-347*   | 461-594*   |

  The p50:

  | Places | `write`   | `write_series` 100 | 1000       | 3000       |
  | ------ | --------- | ------------------ | ---------- | ---------- |
  | 1      | 0.33-0.53 | 0.55-1.15          | 1.44-4.99  | 2.43-6.71  |
  | 16     | 1.19-1.63 | 2.48-7.21          | 5.85-6.09* | 12.8-13.9* |
  | 64     | 3.89-4.04 | 13.9-15.0          | 23.3-32.0* | 96-103*    |
  | 255    | 18.9-21.1 | 28.4-53.6*         | 148-211*   | 396-437*   |

  A cell with `*` is a frame over the largest block (57344 bytes), which never sends:
  since the round 3 fix of PR 3325, its series raise `ValueError` before their copy, and
  the bench skips it. Its numbers are of 3ab8c7246, where it raised at the send. The
  largest frame in the tables that can be sent, 64 places of 100 samples (52000 bytes of
  series), takes 13.9-15.0 at p50 and 28.9-33.2 at p99.

  One place of `bool`, p50 then p99: `write` 0.21-0.26, 0.45-2.13; `write_series`
  0.94-0.99, 2.09-9.91 at 100 samples, 1.27-1.54, 10.4-12.0 at 1000, and 2.19-11.2,
  13.2-27.1 at 3000. One place of `f64` with `datetime64[ns]` stamps: 0.66-0.71,
  1.97-5.81 at 100 samples, 1.18-5.87, 2.98-7.58 at 1000, and 3.03-8.07, 11.2-24.3 at
  3000. The round trip, p50 of 400 calls: `write` of one value 49-58, and
  `write_series` of one place 52-63 at 100 samples, 100-579 at 1000, and 139-726 at
  3000. The noise: the same binary of `cargo bench -p program --bench write` ran
  twice on this box differs by up to 52%, and the passes above differ by up to 6
  times in a p99 at one place and up to 2 times at more places.

  P1 judgment. Over 2.5 µs p99 in each pass: each case of `write_series` in the tables
  but one place of 100 samples, which is over it in 3 of 5 passes, as one place of
  `bool` or with `datetime64[ns]` stamps at 100 samples is in 4 of 5; and `write` at 16,
  64, and 255 places. `write` at one place is over it in 4 of 5 passes, with a p50 of
  0.33 to 0.53. The cost buys the Python surface: a `dict` from names or places to
  Python values or arrays, each checked against its place, and the copy with the GIL
  held of this item. It costs about 60 to 80 ns for each value of `write`, about 0.1 to
  0.2 µs for each array of `write_series` (the cast, the dtype check, and the `numpy`
  borrow), and about 0.2 to 1 ns for each sample that it copies, the stamps of the index
  included. It runs once for each frame that a Python program writes, on the CPU of the
  program, not of the node, and adds 0 bytes per sample. A cut of the part per sample
  needs a copy fewer, the split that this item rejects. A cut of the part per value and
  per array needs no Python objects, which the Python surface cannot give. The table of
  calls that raise before the send, at c502e295f, was accepted by `laptop.architect-2`
  at 2026-10-19T04:32:35Z
  (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6259712733).
  `laptop.architect-2` accepted the warm numbers above at 2026-10-19T12:46:15Z
  (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267540431), on the
  terms that it states there, which replace the terms of 6259712733.
