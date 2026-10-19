"""Times `Reader.__next__` against a node process.

Run it as `write.py` says, with `read.py` in place of `write.py`. It prints, in
microseconds:

- the round trip of a `write_series` of one series of `f64` and its stamps on one
  client to the `__next__` that gives its frame on a second client, for a complete
  and a latest reader, p50 and p99;
- the part of `__next__` after `sdk::read::Reader::next_within` gives the frame: the
  copy of each series, the arrays, the `dict`, and the `Frame`, p50 and p99. The
  reader times it (`Reader._part`), with the count of frames of each case. Each case
  runs twice: each frame read as it comes, and the frames read after their writes,
  with no wait, which keeps the caches and the thread of the program warm. A queued
  case also gives the whole call of `__next__` and the free of its frame, p50 and
  p99;
- the part of queued frames of one sample of `f64` and of `uuid`, which `write`
  sends, as the writer takes no series of `uuid`. The difference is the cost of one
  `uuid.UUID`.
"""

import os
import sys
import time
import uuid

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from write import (  # noqa: E402
    CALLS,
    CHANNELS,
    LARGEST,
    PLACES,
    SAMPLES,
    SENDS,
    Stamps,
    channel,
    connect,
    p50,
    p99,
)
from test_client import Node  # noqa: E402

import foundation  # noqa: E402


# The disk of each core of the node of each case, and the bytes of series that a case
# writes on it. The node does not trim its buffer yet (#160), so a case that fills the
# ring of the shard of the index loses a frame, which no reader then gets.
DISK = 64 << 20
ROOM = 48 << 20
# The bytes that the node keeps for each series besides its samples: about 36,
# measured with 1024 places, with a margin.
SERIES = 48
# The places of the widest case: frames of 1 sample, which a block of the writer holds.
WIDEST = 1024


def frames(names: list[str], samples: int, count: int, dtype=np.float64,
           queued: bool = False,
           mode: str = "complete") -> tuple[list[int], list[int]]:
    """On a new node, writes `count` frames of `samples` samples of `names` and their
    stamps on one client, or fewer when they do not fit in `ROOM`, each read on a
    second client by a reader of `mode` before the next write, after 50 frames to warm
    up. With `queued`, it writes each frame first, then reads them with no wait. Gives
    the round trip, or with `queued` the whole call of `__next__` and the free of its
    frame, and the part of each frame, in nanoseconds. A `dtype` of `uuid.UUID` writes
    each frame with `write`, so takes one name and one sample. Raises `DataLost` when the node lost a frame."""
    width = 16 if dtype is uuid.UUID else np.dtype(dtype).itemsize
    size = (len(names) + 1) * SERIES + samples * (len(names) * width + 8)
    count = min(count, ROOM // size - 50)
    random = np.random.default_rng(3321)
    series = {name: (random.random(samples) < 0.5 if dtype is bool
                     else random.random(samples)) for name in names}
    stamps = Stamps()

    def send(writer: foundation.Writer) -> None:
        if dtype is uuid.UUID:
            writer.write({names[0]: uuid.UUID(int=3321)})
        else:
            writer.write_series(series | {"w.time": stamps.take(samples)})

    trips, parts = [], []
    node = Node(DISK)
    try:
        with (connect(node, max(CHANNELS, len(names))) as writes,
              node.connect() as reads):
            with (reads.open_reader(names, mode=mode) as reader,
                  writes.open_writer(names, on_loss="raise") as writer):
                if queued:
                    for _ in range(count + 50):
                        send(writer)
                for n in range(count + 50):
                    began = time.perf_counter_ns()
                    if not queued:
                        send(writer)
                    next(reader)
                    if n >= 50:
                        trips.append(time.perf_counter_ns() - began)
                        parts.append(reader._part)
    finally:
        node.stop()
    return trips, parts


def main() -> None:
    for mode in ("complete", "latest"):
        for samples in SAMPLES:
            trips, _ = frames([channel(0)], samples, SENDS, mode=mode)
            print(
                f"read, {mode}, {samples} samples: round trip p50 {p50(trips):.1f}, "
                f"p99 {p99(trips):.1f}"
            )
    cases = [(places, samples) for places in PLACES for samples in SAMPLES]
    for places, samples in cases + [(WIDEST, 1)]:
        names = [channel(n) for n in range(places)]
        if (places + 1) * samples * 8 > LARGEST:
            print(
                f"read, {places} places, {samples} samples: over the largest "
                f"block, never sent"
            )
            continue
        for queued in (False, True):
            calls, parts = frames(names, samples, CALLS, queued=queued)
            whole = (f", the whole call p50 {p50(calls):.2f}, p99 {p99(calls):.2f}"
                     if queued else "")
            print(
                f"read, {places} places, {samples} samples"
                f"{', queued' if queued else ''}: {len(parts)} frames, after "
                f"the frame p50 {p50(parts):.2f}, p99 {p99(parts):.2f}{whole}"
            )
    for samples in SAMPLES:
        _, parts = frames(["w.flag"], samples, CALLS, bool)
        print(
            f"read, bool, {samples} samples: {len(parts)} frames, after the frame "
            f"p50 {p50(parts):.2f}, p99 {p99(parts):.2f}"
        )
    for label, name, dtype in (("f64", channel(0), np.float64),
                               ("uuid", "w.key", uuid.UUID)):
        _, parts = frames([name], 1, CALLS, dtype, queued=True)
        print(
            f"read, {label}, 1 sample, queued: {len(parts)} frames, after the frame "
            f"p50 {p50(parts):.2f}, p99 {p99(parts):.2f}"
        )


if __name__ == "__main__":
    main()
