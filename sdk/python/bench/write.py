"""Times `Writer.write` and `Writer.write_series` against a node process.

Run it with the `wheel` build of the package with the feature `sim` on the path,
NumPy installed, and `FOUNDATION_NODE` set to the path of a release `foundation`
binary:

    cargo build -p sdk-python --profile wheel --features sim
    FOUNDATION_NODE=target/release/foundation python3 sdk/python/bench/write.py

It prints, in microseconds:

- the round trip of `write` (one value, node stamps) and of `write_series` (a series
  of `f64` and its stamps), p50 and p99;
- `write` with channel names as keys against places as keys, p50 of the part;
- the part of a call before `sdk::write::Writer::write`: the key lookup, the
  conversion, the copy, and the list of parts, p50 and p99. The writer times it
  (`Writer._part`) on calls that reach the send with warm buffers. It skips series
  over `LARGEST`, which raise `ValueError` before their copy and never send.
"""

import os
import statistics
import subprocess
import sys
import time

import numpy as np

TESTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests")
sys.path.insert(0, TESTS)
from test_client import PATIENCE, Node  # noqa: E402

import foundation  # noqa: E402

CHANNELS = 256
PLACES = (1, 16, 64, 255)
SAMPLES = (100, 1000, 3000)
SENDS = 400
CALLS = 3000
# The largest block of the pool of frames, in bytes.
LARGEST = 57344


def p50(times: list[int]) -> float:
    """The median of `times`, in nanoseconds, in microseconds."""
    return statistics.median(times) / 1e3


def p99(times: list[int]) -> float:
    """The 99th percentile of `times`, in nanoseconds, in microseconds."""
    return statistics.quantiles(times, n=100)[98] / 1e3


def timed(call, count: int) -> list[int]:
    """The time of each of `count` calls of `call`, in nanoseconds, after 50 calls to
    warm up."""
    times = []
    for n in range(count + 50):
        began = time.perf_counter_ns()
        call()
        if n >= 50:
            times.append(time.perf_counter_ns() - began)
    return times


def parted(writer: foundation.Writer, call, count: int) -> list[int]:
    """The part before the send of each of `count` calls of `call` on `writer`, in
    nanoseconds, after 50 calls to warm up. A call may raise at the send."""
    times = []
    for n in range(count + 50):
        try:
            call()
        except (ValueError, foundation.Rejected):
            pass
        if n >= 50:
            times.append(writer._part)
    return times


def ranged(writer: foundation.Writer, call) -> str:
    """The p50 and p99 of the part of `CALLS` calls of `call` on `writer`."""
    times = parted(writer, call, CALLS)
    return f"p50 {p50(times):.2f}, p99 {p99(times):.2f}"


class Stamps:
    """Stamps that go up from call to call, as `int64` or `datetime64[ns]`."""

    def __init__(self, dtype: str = "int64") -> None:
        self.next = time.time_ns() + 1_000_000_000
        self.dtype = dtype

    def take(self, samples: int) -> np.ndarray:
        """The next `samples` stamps."""
        stamps = np.arange(self.next, self.next + samples, dtype=np.int64)
        self.next += samples
        return stamps.astype(self.dtype)


def channel(n: int) -> str:
    """The name of channel `n`."""
    return f"w.c{n:03}"


def connect(node: Node) -> foundation.Client:
    """Plans and applies `CHANNELS` channels of `f64` and one of `bool` on one index
    on `node`, and gives a client of it once the hub of the node knows them."""
    hcl = 'channel "w.time" { kind = "index" }\n'
    hcl += 'channel "w.flag" {\n  data_type = "bool"\n  index = "w.time"\n}\n'
    for n in range(CHANNELS):
        hcl += f'channel "{channel(n)}" {{\n  data_type = "f64"\n'
        hcl += '  index = "w.time"\n}\n'
    hcl += 'placement "w.home" {\n  select = "w.*"\n  home = "edge"\n}\n'
    with open(os.path.join(node.dir.name, "w.hcl"), "w") as file:
        file.write(hcl)
    for args in (["plan", "w.hcl", "--out", "w.plan"], ["apply", "w.plan"]):
        subprocess.run(
            [os.environ["FOUNDATION_NODE"], *args],
            cwd=node.dir.name,
            check=True,
            capture_output=True,
            timeout=PATIENCE,
        )
    client = node.connect()
    deadline = time.monotonic() + PATIENCE
    while True:
        try:
            client.open_writer(channel(CHANNELS - 1)).close()
            return client
        except foundation.Stopped as error:
            if error.code != "unknown" or time.monotonic() > deadline:
                raise
            time.sleep(0.01)


def round_trips(client: foundation.Client) -> None:
    """Prints the round trip of `write` and of `write_series`."""
    random = np.random.default_rng(3320)
    with client.open_writer(channel(0), on_loss="ignore") as writer:
        times = timed(lambda: writer.write(1.5), SENDS)
        print(f"write, 1 value: round trip p50 {p50(times):.1f}, p99 {p99(times):.1f}")
        stamps = Stamps()
        for samples in SAMPLES:
            floats = random.random(samples)

            def send() -> None:
                series = {"w.time": stamps.take(samples), channel(0): floats}
                writer.write_series(series)

            times = timed(send, SENDS)
            print(
                f"write_series, {samples} samples: round trip p50 {p50(times):.1f}, "
                f"p99 {p99(times):.1f}"
            )


def keys(client: foundation.Client) -> None:
    """Prints `write` with names as keys against places as keys."""
    for places in PLACES:
        names = [channel(n) for n in range(places)]
        with client.open_writer(names, on_loss="ignore") as writer:
            by_name = {name: 1.5 for name in names}
            by_place = {n: 1.5 for n in range(places)}
            name = p50(parted(writer, lambda: writer.write(by_name), CALLS))
            place = p50(parted(writer, lambda: writer.write(by_place), CALLS))
            print(
                f"write, {places} places: names p50 {name:.2f}, places p50 "
                f"{place:.2f}, names less places {name - place:.2f}"
            )


def parts(client: foundation.Client) -> None:
    """Prints the part of `write` and of `write_series` before the call of `sdk`."""
    random = np.random.default_rng(3320)
    for places in PLACES:
        names = [channel(n) for n in range(places)]
        with client.open_writer(names, on_loss="ignore") as writer:
            values = {name: 1.5 for name in names}
            part = ranged(writer, lambda: writer.write(values))
            print(f"write, {places} places: before the send {part}")
            stamps = Stamps()
            for samples in SAMPLES:
                if (places + 1) * samples * 8 > LARGEST:
                    print(
                        f"write_series, {places} places, {samples} samples: over the "
                        f"largest block, never sent"
                    )
                    continue
                series = {name: random.random(samples) for name in names}

                def call() -> None:
                    writer.write_series(series | {"w.time": stamps.take(samples)})

                part = ranged(writer, call)
                print(
                    f"write_series, {places} places, {samples} samples: before the "
                    f"send {part}"
                )
    with client.open_writer(["w.flag"], on_loss="ignore") as writer:
        part = ranged(writer, lambda: writer.write(True))
        print(f"write, bool: before the send {part}")
        stamps = Stamps()
        for samples in SAMPLES:
            flags = random.random(samples) < 0.5

            def call() -> None:
                writer.write_series({"w.flag": flags, "w.time": stamps.take(samples)})

            part = ranged(writer, call)
            print(f"write_series, bool, {samples} samples: before the send {part}")
    with client.open_writer(channel(0), on_loss="ignore") as writer:
        stamps = Stamps("datetime64[ns]")
        for samples in SAMPLES:
            floats = random.random(samples)

            def call() -> None:
                series = {channel(0): floats, "w.time": stamps.take(samples)}
                writer.write_series(series)

            part = ranged(writer, call)
            print(
                f"write_series, datetime64, {samples} samples: before the send {part}"
            )


def main() -> None:
    node = Node()
    try:
        client = connect(node)
        round_trips(client)
        keys(client)
        parts(client)
        client.close()
    finally:
        node.stop()


if __name__ == "__main__":
    main()
