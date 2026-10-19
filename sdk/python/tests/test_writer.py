"""The writer of the Python SDK against a node process.

`FOUNDATION_NODE` is the path of the `foundation` binary that each test starts.
"""

import gc
import logging
import os
import subprocess
import sys
import threading
import time
import unittest
import uuid

import numpy as np

import foundation
from test_client import PATIENCE, Node, fork

CHANNELS = """
channel "plant.time" { kind = "index" }
channel "plant.a" {
  data_type = "f64"
  index = "plant.time"
}
channel "plant.b" {
  data_type = "i64"
  index = "plant.time"
}
channel "plant.c" {
  data_type = "u8"
  index = "plant.time"
}
channel "plant.d" {
  data_type = "bool"
  index = "plant.time"
}
channel "plant.e" {
  data_type = "uuid"
  index = "plant.time"
}
channel "plant.f" {
  data_type = "f32"
  index = "plant.time"
}
channel "plant.g" {
  data_type = "duration"
  index = "plant.time"
}
channel "other.time" { kind = "index" }
channel "other.a" {
  data_type = "f64"
  index = "other.time"
}
placement "plant.home" {
  select = "plant.*"
  home = "edge"
}
placement "other.home" {
  select = "other.*"
  home = "edge"
}
"""
UNKNOWN = (
    "the selector matches no channel, or a channel of the read or write is unknown or "
    "was removed"
)
ORDER = "the node refused the frame: a stamp is not after the stamp before it"


def planted(node: Node) -> foundation.Client:
    """Plans and applies `CHANNELS` on `node`, and gives a client of it once the hub
    of the node knows the channels, which it learns after the apply."""
    with open(os.path.join(node.dir.name, "plant.hcl"), "w") as file:
        file.write(CHANNELS)
    for args in (["plan", "plant.hcl", "--out", "plant.plan"], ["apply", "plant.plan"]):
        done = subprocess.run(
            [os.environ["FOUNDATION_NODE"], *args],
            cwd=node.dir.name,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=PATIENCE,
        )
        if done.returncode != 0 or done.stderr:
            raise AssertionError(
                f"foundation {' '.join(args)} exited with {done.returncode}:\n"
                + done.stderr.decode()
            )
    client = node.connect()
    deadline = time.monotonic() + PATIENCE
    while True:
        try:
            client.open_writer("plant.a").close()
            client.open_writer("other.a").close()
            return client
        except foundation.Stopped as error:
            if error.code != "unknown" or time.monotonic() > deadline:
                raise
            time.sleep(0.01)


class Selfish(np.ndarray):
    """An array whose `copy` and `view` give the array itself."""

    def copy(self, *args, **kwargs) -> "Selfish":
        return self

    def view(self, *args, **kwargs) -> "Selfish":
        return self


class WriterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.node = Node()
        self.addCleanup(self.node.stop)
        self.client = planted(self.node)
        self.addCleanup(self.client.close)

    def test_a_writer_gives_its_places_in_the_order_of_its_channels(self) -> None:
        with self.client.open_writer(["plant.b", "plant.a"]) as writer:
            self.assertEqual(
                writer.places,
                (
                    ("plant.b", "i64"),
                    ("plant.a", "f64"),
                    ("plant.time", "timestamp"),
                ),
            )
            self.assertEqual(writer.index, 2)
        with self.client.open_writer(["plant.time", "plant.a"]) as writer:
            self.assertEqual(writer.index, 0)

    def test_write_takes_frames_of_node_time_and_of_the_callers_stamps(self) -> None:
        with self.client.open_writer(["plant.a", "plant.b"]) as writer:
            self.assertIsNone(writer.write({"plant.a": 1.5, "plant.b": 2}))
            stamp = time.time_ns() + 10**9
            self.assertIsNone(writer.write({"plant.a": 2.5, 1: 3, 2: stamp}))
            with self.assertRaises(foundation.Rejected) as raised:
                writer.write({"plant.a": 3.5, "plant.time": stamp})
            self.assertEqual(raised.exception.code, "order")
            self.assertEqual(str(raised.exception), ORDER)
            self.assertIsNone(writer.write({"plant.a": 3.5, "plant.time": stamp + 1}))

    def test_write_takes_a_value_with_no_key_for_one_place(self) -> None:
        with self.client.open_writer("plant.a") as writer:
            self.assertIsNone(writer.write(2.5))
            self.assertIsNone(writer.write(3))
        with self.client.open_writer(["plant.a", "plant.b"]) as writer:
            with self.assertRaises(TypeError) as raised:
                writer.write(2.5)
            self.assertEqual(
                str(raised.exception),
                "the writer has 2 places other than the index: pass a dict from each "
                "channel name to its value",
            )

    def test_write_takes_a_value_of_each_scalar(self) -> None:
        channels = ["plant.b", "plant.c", "plant.d", "plant.e", "plant.f", "plant.g"]
        with self.client.open_writer(channels) as writer:
            values = {
                "plant.b": -(2**63),
                "plant.c": 255,
                "plant.d": True,
                "plant.e": uuid.uuid4(),
                "plant.f": 1.5,
                "plant.g": 2**63 - 1,
            }
            self.assertIsNone(writer.write(values))

    def test_write_raises_for_each_value_that_its_place_does_not_take(self) -> None:
        channels = ["plant.a", "plant.b", "plant.c", "plant.d", "plant.e", "plant.f"]
        writer = self.client.open_writer(channels)
        self.addCleanup(writer.close)
        big = 2**200
        flag = type(np.True_).__name__
        cases = [
            ({"plant.a": "1.5"}, TypeError, "plant.a holds f64, not str"),
            ({"plant.a": True}, TypeError, "plant.a holds f64, not bool"),
            ({"plant.a": np.True_}, TypeError, f"plant.a holds f64, not {flag}"),
            ({"plant.f": np.True_}, TypeError, f"plant.f holds f32, not {flag}"),
            ({"plant.b": np.True_}, TypeError, f"plant.b holds i64, not {flag}"),
            ({"plant.c": np.False_}, TypeError, f"plant.c holds u8, not {flag}"),
            (
                {"plant.time": np.True_},
                TypeError,
                f"plant.time holds timestamp, not {flag}",
            ),
            ({"plant.b": 1.5}, TypeError, "plant.b holds i64, not float"),
            ({"plant.b": False}, TypeError, "plant.b holds i64, not bool"),
            ({"plant.d": 1}, TypeError, "plant.d holds bool, not int"),
            ({"plant.e": str(uuid.uuid4())}, TypeError, "plant.e holds uuid, not str"),
            ({"plant.time": 1.0}, TypeError, "plant.time holds timestamp, not float"),
            (
                {"plant.c": 300},
                OverflowError,
                "plant.c holds u8, and 300 is outside 0 to 255",
            ),
            ({"plant.c": -1}, OverflowError, "plant.c holds u8, and -1 is outside 0 to 255"),
            (
                {"plant.b": big},
                OverflowError,
                f"plant.b holds i64, and {big} is outside -9223372036854775808 to "
                "9223372036854775807",
            ),
            (
                {"plant.f": 1e39},
                OverflowError,
                "plant.f holds f32, and 1e+39 is outside its range",
            ),
            (
                {"plant.a": 2**1024},
                OverflowError,
                f"plant.a holds f64, and {2**1024} is outside its range",
            ),
            (
                {"plant.z": 1.0},
                KeyError,
                "the writer has no place plant.z: its places are plant.a, plant.b, "
                "plant.c, plant.d, plant.e, plant.f, plant.time",
            ),
            ({7: 1.0}, ValueError, "the writer has no place 7: it has 7 places"),
            ({9: 1.0}, ValueError, "the writer has no place 9: it has 7 places"),
            ({-1: 1.0}, ValueError, "the writer has no place -1: it has 7 places"),
            ({1.5: 1.0}, TypeError, "a key is a channel name or a place, not float"),
            (
                {"plant.a": 1.0, 0: 2.0},
                ValueError,
                "the series of the draft are not valid: entry 0 follows entry 0: "
                "entries must increase",
            ),
            ({}, ValueError, "the frame holds no sample, so it was not sent"),
        ]
        for values, kind, text in cases:
            with self.subTest(values=values):
                with self.assertRaises(kind) as raised:
                    writer.write(values)
                self.assertIs(type(raised.exception), kind)
                self.assertEqual(raised.exception.args[0], text)
        self.assertIsNone(writer.write({"plant.a": np.float64(1.5), "plant.c": 0}))
        self.assertIsNone(writer.write({"plant.f": float("nan")}))

    def test_write_series_takes_each_dtype_of_its_places(self) -> None:
        count = 1000
        start = time.time_ns()
        stamps = np.arange(start, start + count, dtype=np.int64)
        with self.client.open_writer(["plant.a", "plant.c", "plant.d"]) as writer:
            series = {
                "plant.a": np.linspace(0.0, 1.0, count),
                "plant.c": np.arange(count, dtype=np.uint8),
                "plant.d": np.arange(count) % 2 == 0,
                "plant.time": stamps,
            }
            self.assertIsNone(writer.write_series(series))
            later = (stamps + count).astype("datetime64[ns]")
            series["plant.time"] = later
            self.assertIsNone(writer.write_series(series))
            strided = {key: array[::2] for key, array in series.items()}
            strided["plant.time"] = (stamps + 2 * count)[::2]
            self.assertIsNone(writer.write_series(strided))
            with self.assertRaises(foundation.Rejected) as raised:
                writer.write_series(strided)
            self.assertEqual(raised.exception.code, "order")
            self.assertEqual(str(raised.exception), ORDER)
        with self.client.open_writer("plant.g") as writer:
            spans = np.arange(count, dtype=np.int64).astype("timedelta64[ns]")
            stamps = stamps + 3 * count
            self.assertIsNone(writer.write_series({0: spans, 1: stamps}))

    def test_write_series_raises_for_each_array_that_its_place_does_not_take(
        self,
    ) -> None:
        writer = self.client.open_writer(["plant.a", "plant.e"])
        self.addCleanup(writer.close)
        start = time.time_ns()
        stamps = np.arange(start, start + 100, dtype=np.int64)
        floats = np.zeros(100)
        cases = [
            (
                {"plant.a": floats.astype(np.float32), "plant.time": stamps},
                TypeError,
                'plant.a holds f64 and takes an array of float64, not float32: pass '
                'a.astype("float64")',
            ),
            (
                {"plant.a": floats.astype(">f8"), "plant.time": stamps},
                TypeError,
                'plant.a holds f64 and takes an array of float64, not >f8: pass '
                'a.astype("float64")',
            ),
            (
                {"plant.a": floats, "plant.time": stamps.astype("datetime64[s]")},
                TypeError,
                "plant.time holds timestamp and takes an array of int64 or "
                'datetime64[ns], not datetime64[s]: pass a.astype("datetime64[ns]")',
            ),
            (
                {"plant.a": floats, "plant.time": stamps.astype(np.int32)},
                TypeError,
                "plant.time holds timestamp and takes an array of int64 or "
                'datetime64[ns], not int32: pass a.astype("int64")',
            ),
            (
                {"plant.a": floats.reshape(10, 10), "plant.time": stamps},
                TypeError,
                "plant.a takes an array of one dimension, not 2",
            ),
            (
                {"plant.a": [0.0] * 100, "plant.time": stamps},
                TypeError,
                "plant.a takes a numpy.ndarray, not list",
            ),
            (
                {"plant.e": np.zeros(100, dtype=np.uint64), "plant.time": stamps},
                TypeError,
                "plant.e holds uuid, which the writer takes no series of",
            ),
            (
                {"plant.a": floats, "plant.time": stamps[:99]},
                ValueError,
                "the series of plant.time holds 99 samples, not the 100 of plant.a",
            ),
            (
                {"plant.a": floats},
                ValueError,
                "the series of the draft are not valid: entry 0 is present but its "
                "index, entry 2, is absent",
            ),
            ({}, ValueError, "the frame holds no sample, so it was not sent"),
        ]
        for series, kind, text in cases:
            with self.subTest(series=list(series)):
                with self.assertRaises(kind) as raised:
                    writer.write_series(series)
                self.assertIs(type(raised.exception), kind)
                self.assertEqual(str(raised.exception), text)
        # Series of the bytes of one frame, which its head puts over the largest block.
        large = {"plant.a": np.zeros(3584), "plant.time": np.arange(3584) + start}
        with self.assertRaises(ValueError) as raised:
            writer.write_series(large)
        self.assertRegex(
            str(raised.exception),
            r"^no block of the pool holds a message or a frame: block of \d+ bytes is "
            r"above the largest block of 57344 bytes$",
        )
        self.assertIsNone(writer.write_series({"plant.a": floats, "plant.time": stamps}))

    def test_open_writer_raises_for_each_argument_that_it_does_not_take(self) -> None:
        cases = [
            ([], {}, ValueError, "the writer names no channel"),
            ([5], {}, TypeError, "a channel name is a str, not int"),
            (
                "plant.a",
                {"authority": 256},
                ValueError,
                "authority is 256, outside 0 to 255",
            ),
            (
                "plant.a",
                {"authority": -1},
                ValueError,
                "authority is -1, outside 0 to 255",
            ),
            (
                "plant.a",
                {"lease": 0.0},
                ValueError,
                "lease is 0.0 s, not a finite number of seconds over 0",
            ),
            (
                "plant.a",
                {"lease": 1e-12},
                ValueError,
                "lease is 1e-12 s, not a finite number of seconds over 0",
            ),
            (
                "plant.a",
                # Its nanoseconds round to 2^63, one past the largest i64.
                {"lease": 2**63 / 1e9},
                ValueError,
                "lease is 9223372036.854776 s, not a finite number of seconds over 0",
            ),
            (
                "plant.a",
                {"lease": float("inf")},
                ValueError,
                "lease is inf s, not a finite number of seconds over 0",
            ),
            (
                "plant.a",
                {"lease": float("nan")},
                ValueError,
                "lease is NaN s, not a finite number of seconds over 0",
            ),
            (
                "plant.a",
                {"on_loss": "drop"},
                ValueError,
                'on_loss is "drop", not "warn", "raise", or "ignore"',
            ),
        ]
        for channels, arguments, kind, text in cases:
            with self.subTest(channels=channels, arguments=arguments):
                with self.assertRaises(kind) as raised:
                    self.client.open_writer(channels, **arguments)
                self.assertIs(type(raised.exception), kind)
                self.assertEqual(str(raised.exception), text)
        with self.assertRaises(ValueError) as raised:
            self.client.open_writer("plant a")
        self.assertRegex(str(raised.exception), r"^channels: ")
        with self.assertRaises(TypeError):
            self.client.open_writer(5)
        for authority, kind in [("1", "str"), (None, "NoneType")]:
            with self.subTest(authority=authority):
                with self.assertRaises(TypeError) as raised:
                    self.client.open_writer("plant.a", authority=authority)
                text = f"authority is an int, not {kind}"
                self.assertEqual(str(raised.exception), text)
        writer = self.client.open_writer(("plant.a",), authority=255, lease=5.0)
        self.assertIsNone(writer.close())

    def test_open_writer_raises_stopped_with_the_code_of_the_node(self) -> None:
        with self.assertRaises(foundation.Stopped) as raised:
            self.client.open_writer(["plant.a", "plant.z"])
        self.assertNotIsInstance(raised.exception, ConnectionError)
        self.assertEqual(raised.exception.code, "unknown")
        self.assertEqual(
            str(raised.exception), f"{UNKNOWN}: the open names plant.a, plant.z"
        )
        with self.assertRaises(foundation.Stopped) as raised:
            self.client.open_writer(["plant.a", "other.a"])
        self.assertEqual(raised.exception.code, "malformed")
        self.assertIsNone(self.client.open_writer("plant.a").close())

    def test_a_writer_of_a_closed_client_raises(self) -> None:
        writer = self.client.open_writer("plant.a")
        self.client.close()
        with self.assertRaises(RuntimeError) as raised:
            self.client.open_writer("plant.a")
        self.assertEqual(str(raised.exception), "the client is closed")
        with self.assertRaises(RuntimeError) as raised:
            writer.write(1.0)
        self.assertEqual(str(raised.exception), "the client is closed")

    def test_a_closed_writer_raises_and_closes_again_with_none(self) -> None:
        with self.client.open_writer("plant.a") as writer:
            writer.write(1.0)
        self.assertIsNone(writer.close())
        for call in (lambda: writer.write(1.0), lambda: writer.write_series({})):
            with self.assertRaises(RuntimeError) as raised:
                call()
            self.assertEqual(str(raised.exception), "the writer is closed")

    def test_a_close_during_the_conversion_of_a_value_ends_the_write(self) -> None:
        writer = self.client.open_writer("plant.a")
        converting = threading.Event()
        closed = []

        class Value:
            def __float__(self) -> float:
                converting.set()
                thread.join(PATIENCE)
                return 1.0

        def close() -> None:
            converting.wait(PATIENCE)
            closed.append(writer.close())

        thread = threading.Thread(target=close, daemon=True)
        thread.start()
        with self.assertRaises(RuntimeError) as raised:
            writer.write(Value())
        self.assertEqual(str(raised.exception), "the writer is closed")
        self.assertEqual(closed, [None])

    def test_two_threads_that_write_on_one_writer_both_finish(self) -> None:
        writer = self.client.open_writer("plant.a")
        self.addCleanup(writer.close)
        raised = []

        def write() -> None:
            try:
                for value in range(300):
                    writer.write(float(value))
            except BaseException as error:
                raised.append(error)

        threads = [threading.Thread(target=write, daemon=True) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(PATIENCE)
            self.assertFalse(thread.is_alive())
        self.assertEqual(raised, [])

    def waits(self, call) -> object:
        """Pauses the node, calls `call`, and asserts that a thread that counts for
        0.2 s and then continues the node ran while `call` waited. Gives what `call`
        gives."""
        self.node.pause()
        # Continues the node when a call that keeps the GIL stops each thread.
        backstop = subprocess.Popen(
            ["sh", "-c", f"sleep 10; kill -CONT {self.node.process.pid}"]
        )
        self.addCleanup(backstop.wait)
        self.addCleanup(backstop.kill)
        counts = []

        def counter() -> None:
            deadline = time.monotonic() + 0.2
            count = 0
            while time.monotonic() < deadline:
                count += 1
                time.sleep(0)
            counts.append(count)
            self.node.resume()

        thread = threading.Thread(target=counter, daemon=True)
        thread.start()
        given = call()
        thread.join(PATIENCE)
        self.assertGreater(counts[0], 20)
        return given

    def test_other_threads_run_while_a_write_waits(self) -> None:
        with self.client.open_writer("plant.a") as writer:
            self.assertIsNone(self.waits(lambda: writer.write(1.5)))

    def test_other_threads_run_while_a_write_series_waits(self) -> None:
        series = {"plant.a": np.ones(3), "plant.time": np.arange(3) + time.time_ns()}
        with self.client.open_writer("plant.a") as writer:
            self.assertIsNone(self.waits(lambda: writer.write_series(series)))

    def test_other_threads_run_while_a_close_waits(self) -> None:
        writer = self.client.open_writer("plant.a")
        self.assertIsNone(self.waits(writer.close))

    def test_other_threads_run_while_an_open_waits(self) -> None:
        writer = self.waits(lambda: self.client.open_writer("plant.a"))
        self.assertIsNone(writer.close())

    def test_a_writer_of_low_authority_waits_while_one_of_higher_holds_control(
        self,
    ) -> None:
        with self.client.open_writer("plant.a") as low:
            with self.client.open_writer("plant.a", authority=255) as high:
                self.assertIsNone(high.write(1.5))
                with self.assertRaises(foundation.Rejected) as raised:
                    low.write(2.5)
                self.assertEqual(raised.exception.code, "waiting")
                self.assertEqual(
                    str(raised.exception),
                    "the node refused the frame: another writer holds control",
                )

    def test_a_write_after_the_lease_ran_out_raises_expired(self) -> None:
        with self.client.open_writer("plant.a", lease=0.05) as writer:
            time.sleep(1)
            with self.assertRaises(foundation.Rejected) as raised:
                writer.write(1.5)
            self.assertEqual(raised.exception.code, "expired")
            self.assertEqual(
                str(raised.exception),
                "the node refused the frame: the writer's control lease ran out: "
                "open the writer again",
            )

    def test_a_forked_child_does_not_own_the_writer(self) -> None:
        writer = self.client.open_writer("plant.a")
        self.addCleanup(writer.close)
        parent = os.getpid()

        def child() -> object:
            text = (
                f"the client connected in process {parent}, and this is process "
                f"{os.getpid()}: connect again in this process"
            )
            calls = [
                lambda: writer.write(1.0),
                lambda: self.client.open_writer("plant.a"),
            ]
            for call in calls:
                try:
                    call()
                except RuntimeError as error:
                    if str(error) != text:
                        return str(error)
                else:
                    return "the child wrote or opened"
            return writer.close()

        self.assertEqual(fork(child), 0)
        self.assertIsNone(writer.write(2.0))

    def test_a_child_that_drops_an_open_writer_touches_no_descriptor(self) -> None:
        writers = [self.client.open_writer("plant.a")]

        def drop() -> object:
            raised = []
            sys.unraisablehook = raised.append
            os.closerange(3, 65536)
            writers.clear()
            gc.collect()
            return raised or None

        self.assertEqual(fork(drop), 0)
        self.assertIsNone(writers[0].close())

    def test_a_lost_frame_raises_warns_or_passes_as_on_loss_says(self) -> None:
        random = np.random.default_rng(3320)
        count = 3000
        frames = [0]
        now = time.time_ns()

        def series() -> dict:
            start = now + frames[0] * count
            frames[0] += 1
            return {
                "plant.a": random.random(count),
                "plant.time": np.arange(start, start + count, dtype=np.int64),
            }

        writer = self.client.open_writer("plant.a", on_loss="raise")
        self.addCleanup(writer.close)
        for _ in range(2000):
            try:
                writer.write_series(series())
            except foundation.DataLost as error:
                lost = error
                break
        else:
            self.fail("2000 frames of 48 kB lost no sample")
        seqs = lost.seqs
        self.assertEqual(list(seqs), ["plant.a", "plant.time"])
        self.assertEqual(len(seqs["plant.a"]), count)
        self.assertEqual(seqs["plant.a"], seqs["plant.time"])
        self.assertEqual(
            str(lost), f"samples were lost: plant.a ({count}), plant.time ({count})"
        )
        self.assertIsNone(writer.close())
        writer = self.client.open_writer("plant.a", on_loss="warn")
        with self.assertLogs("foundation", "WARNING") as logs:
            self.assertIsNone(writer.write_series(series()))
        self.assertEqual(
            logs.output,
            [f"WARNING:foundation:samples were lost: plant.a ({count}), plant.time "
             f"({count})"],
        )
        self.assertIsNone(writer.close())
        writer = self.client.open_writer("plant.a", on_loss="ignore")
        self.assertIsNone(writer.write_series(series()))
        self.assertIsNone(writer.close())

    def test_a_lost_write_names_its_channel_and_the_index(self) -> None:
        stopped = threading.Event()

        def flood() -> None:
            random = np.random.default_rng(3320)
            start = time.time_ns()
            with self.client.open_writer("other.a", on_loss="ignore") as writer:
                while not stopped.is_set():
                    stamps = np.arange(start, start + 3000, dtype=np.int64)
                    start += 3000
                    series = {"other.a": random.random(3000), "other.time": stamps}
                    writer.write_series(series)

        thread = threading.Thread(target=flood, daemon=True)
        thread.start()
        self.addCleanup(thread.join, PATIENCE)
        self.addCleanup(stopped.set)
        with self.client.open_writer("plant.a", on_loss="raise") as writer:
            deadline = time.monotonic() + PATIENCE
            with self.assertRaises(foundation.DataLost) as raised:
                while time.monotonic() < deadline:
                    writer.write(1.5)
        self.assertEqual(
            str(raised.exception), "samples were lost: plant.a (1), plant.time (1)"
        )

    def test_a_write_from_a_value_does_not_hang(self) -> None:
        writer = self.client.open_writer("plant.a")
        raised = []

        class Value:
            def __float__(self) -> float:
                writer.write(1.0)
                return 2.0

        def write() -> None:
            try:
                writer.write(Value())
            except BaseException as error:
                raised.append(error)

        thread = threading.Thread(target=write, daemon=True)
        thread.start()
        thread.join(10)
        self.assertFalse(thread.is_alive(), "a write from __float__ hangs")
        self.assertEqual(raised, [])
        self.assertIsNone(writer.close())

    def test_a_logging_handler_that_writes_on_a_lost_frame_does_not_hang(self) -> None:
        random = np.random.default_rng(3320)
        count = 3000
        now = time.time_ns()
        writer = self.client.open_writer("plant.a")
        written = []

        class Handler(logging.Handler):
            def emit(self, record: logging.LogRecord) -> None:
                if not written:
                    written.append(record.getMessage())
                    written.append(writer.write(2.5))

        handler = Handler()
        logger = logging.getLogger("foundation")
        logger.addHandler(handler)
        self.addCleanup(logger.removeHandler, handler)
        raised = []

        def write() -> None:
            try:
                for frame in range(2000):
                    start = now + frame * count
                    writer.write_series({
                        "plant.a": random.random(count),
                        "plant.time": np.arange(start, start + count, dtype=np.int64),
                    })
                    if written:
                        return
            except BaseException as error:
                raised.append(error)

        thread = threading.Thread(target=write, daemon=True)
        thread.start()
        thread.join(PATIENCE)
        self.assertFalse(thread.is_alive(), "a write from a logging handler hangs")
        self.assertEqual(raised, [])
        self.assertEqual(
            written,
            [f"samples were lost: plant.a ({count}), plant.time ({count})", None],
        )
        self.assertIsNone(writer.close())

    def test_write_series_takes_an_array_that_is_not_aligned_or_packed(self) -> None:
        start = time.time_ns()
        unaligned = np.frombuffer(bytes(8 * 4 + 1), dtype=np.float64, offset=1)
        self.assertFalse(unaligned.flags.aligned)
        wide = np.zeros(4, [("b", "<f8"), ("a", "u1", (4,))])["b"]
        packed = np.zeros(4, [("a", "u1"), ("b", "<f8")])["b"]
        with self.client.open_writer("plant.a") as writer:
            for frame, floats in enumerate([unaligned, wide, packed]):
                stamps = np.arange(4, dtype=np.int64) + start + frame * 4
                series = {"plant.a": floats, "plant.time": stamps}
                self.assertIsNone(writer.write_series(series))

    def test_write_series_takes_a_strided_array(self) -> None:
        start = time.time_ns()
        floats = np.arange(8, dtype=np.float64)[::2]
        stamps = np.arange(16, dtype=np.int64)[::4] + start
        with self.client.open_writer("plant.a") as writer:
            series = {"plant.a": floats, "plant.time": stamps}
            self.assertIsNone(writer.write_series(series))

    def test_write_series_calls_no_method_of_a_subclass_of_ndarray(self) -> None:
        start = time.time_ns()
        raw = bytes(1) + np.arange(4, dtype="<f8").tobytes()
        unaligned = np.frombuffer(raw, dtype=np.float64, offset=1).view(Selfish)
        flags = np.array([True, False, True, True]).view(Selfish)
        with self.client.open_writer(["plant.a", "plant.d"]) as writer:
            stamps = np.arange(4, dtype=np.int64) + start
            series = {"plant.a": unaligned, "plant.d": flags, "plant.time": stamps}
            self.assertIsNone(writer.write_series(series))

    def test_write_series_raises_for_more_samples_than_a_frame_holds(self) -> None:
        count = 2**40
        series = {
            "plant.a": np.broadcast_to(np.zeros(1), count),
            "plant.time": np.broadcast_to(np.zeros(1, np.int64), count),
        }
        with self.client.open_writer("plant.a") as writer:
            with self.assertRaises(ValueError) as raised:
                writer.write_series(series)
        self.assertEqual(
            str(raised.exception),
            f"the series hold {count} samples, over the 4294967295 of one frame",
        )

    def test_write_reads_the_dict_as_it_was_at_the_call(self) -> None:
        values = {}

        class Value:
            def __float__(self) -> float:
                values["plant.b"] = "not an int"
                return 1.0

        values["plant.a"] = Value()
        with self.client.open_writer(["plant.a", "plant.b"]) as writer:
            self.assertIsNone(writer.write(values))
        self.assertEqual(list(values), ["plant.a", "plant.b"])

    def test_a_write_reads_its_dict_after_it_waits_for_a_write_that_sends(
        self,
    ) -> None:
        writer = self.client.open_writer("plant.a")
        self.addCleanup(writer.close)
        values = {"plant.a": 2.0}
        raised = []

        def second() -> None:
            try:
                writer.write(values)
            except TypeError as error:
                raised.append(str(error))

        self.node.pause()
        # Continues the node when a call that keeps the GIL stops each thread.
        backstop = subprocess.Popen(
            ["sh", "-c", f"sleep 10; kill -CONT {self.node.process.pid}"]
        )
        self.addCleanup(backstop.wait)
        self.addCleanup(backstop.kill)
        threads = [
            threading.Thread(target=writer.write, args=(1.0,), daemon=True),
            threading.Thread(target=second, daemon=True),
        ]
        for thread in threads:
            thread.start()
            # The first sends to the paused node with the lock held, and the second
            # waits for the lock.
            time.sleep(0.2)
        self.assertTrue(all(thread.is_alive() for thread in threads))
        values["plant.a"] = "x"
        self.node.resume()
        for thread in threads:
            thread.join(PATIENCE)
            self.assertFalse(thread.is_alive())
        self.assertEqual(raised, ["plant.a holds f64, not str"])

    def test_write_series_reads_the_dict_as_it_was_at_the_call(self) -> None:
        count = 3000
        raw = bytes(1) + np.zeros(count).tobytes()
        unaligned = np.frombuffer(raw, dtype=np.float64, offset=1)
        series = {"plant.a": unaligned, "plant.time": np.arange(7000)}
        done = threading.Event()

        def change() -> None:
            ints = np.zeros(count, np.int64)
            while not done.is_set():
                if series.pop("plant.b", None) is None:
                    series["plant.b"] = ints

        # The thread changes the dict between the calls.
        thread = threading.Thread(target=change)
        with self.client.open_writer(["plant.a", "plant.b"]) as writer:
            thread.start()
            try:
                for _ in range(100):
                    with self.assertRaises(ValueError) as raised:
                        writer.write_series(series)
                    self.assertEqual(
                        str(raised.exception),
                        "the series hold 7000 samples, 80000 bytes, over the 57344 "
                        "bytes of one frame",
                    )
            finally:
                done.set()
                thread.join()

    def test_write_takes_a_numpy_bool_for_a_bool_place(self) -> None:
        with self.client.open_writer("plant.d") as writer:
            for flag in np.array([False, True]):
                self.assertIsInstance(flag, np.bool_)
                self.assertIsNone(writer.write(flag))


if __name__ == "__main__":
    unittest.main()
