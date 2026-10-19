"""Times `Reader.__next__` against a node process.

Run it as `write.py` says, with `read.py` in place of `write.py`. It prints, in
microseconds:

- the round trip of a `write_series` of one series of `f64` and its stamps on one
  client to the `__next__` that gives its frame on a second client, p50 and p99;
- the part of `__next__` after `sdk::read::Reader::next_within` gives the frame: the
  copy of each series, the arrays, the `dict`, and the `Frame`, p50 and p99. The
  reader times it (`Reader._part`).
"""

import os
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from write import (  # noqa: E402
    CALLS,
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


def frames(writes: foundation.Client, reads: foundation.Client, names: list[str],
           samples: int, count: int, dtype=np.float64) -> tuple[list[int], list[int]]:
    """Writes `count` frames of `samples` samples of `names` on `writes`, each read on
    `reads` before the next write, after 50 frames to warm up. Gives the round trip
    and the part of each, in nanoseconds."""
    random = np.random.default_rng(3321)
    series = {name: (random.random(samples) < 0.5 if dtype is bool
                     else random.random(samples)) for name in names}
    stamps = Stamps()
    trips, parts = [], []
    with reads.open_reader(names) as reader, writes.open_writer(names) as writer:
        for n in range(count + 50):
            began = time.perf_counter_ns()
            writer.write_series(series | {"w.time": stamps.take(samples)})
            next(reader)
            if n >= 50:
                trips.append(time.perf_counter_ns() - began)
                parts.append(reader._part)
    return trips, parts


def main() -> None:
    node = Node()
    try:
        writes = connect(node)
        reads = node.connect()
        for samples in SAMPLES:
            trips, _ = frames(writes, reads, [channel(0)], samples, SENDS)
            print(
                f"read, {samples} samples: round trip p50 {p50(trips):.1f}, "
                f"p99 {p99(trips):.1f}"
            )
        for places in PLACES:
            names = [channel(n) for n in range(places)]
            for samples in SAMPLES:
                if (places + 1) * samples * 8 > LARGEST:
                    print(
                        f"read, {places} places, {samples} samples: over the largest "
                        f"block, never sent"
                    )
                    continue
                _, parts = frames(writes, reads, names, samples, CALLS)
                print(
                    f"read, {places} places, {samples} samples: after the frame p50 "
                    f"{p50(parts):.2f}, p99 {p99(parts):.2f}"
                )
        for samples in SAMPLES:
            _, parts = frames(writes, reads, ["w.flag"], samples, CALLS, bool)
            print(
                f"read, bool, {samples} samples: after the frame p50 {p50(parts):.2f}, "
                f"p99 {p99(parts):.2f}"
            )
        reads.close()
        writes.close()
    finally:
        node.stop()


if __name__ == "__main__":
    main()
