"""The reader of the Python SDK against a node process.

`FOUNDATION_NODE` is the path of the `foundation` binary that each test starts.
"""

import _thread
import collections.abc
import gc
import logging
import logging.handlers
import os
import signal
import sys
import threading
import time
import unittest
import uuid

import numpy as np

import foundation
from test_client import PATIENCE, Node, fork, home
from test_stubs import assert_stated, called
from test_writer import CHANNELS, UNKNOWN, planted

# A channel of each other type on the index of `plant.a`.
MORE = "".join(
    f'channel "plant.{kind}" {{\n  data_type = "{kind}"\n  index = "plant.time"\n}}\n'
    for kind in (
        "i8", "i16", "i32", "u16", "u32", "u64", "timestamp", "string",
    )
)
COUNT = 3000


def setUpModule() -> None:
    home(unittest.addModuleCleanup)


class Stamps:
    """Stamps that go up from frame to frame."""

    def __init__(self) -> None:
        self.next = time.time_ns()

    def take(self, count: int) -> np.ndarray:
        """The next `count` stamps."""
        stamps = np.arange(self.next, self.next + count, dtype=np.int64)
        self.next += count
        return stamps


class ReaderTest(unittest.TestCase):
    def setUp(self) -> None:
        self.node = Node()
        self.addCleanup(self.node.stop)
        self.client = planted(self.node, CHANNELS + MORE)
        self.addCleanup(self.client.close)
        # The writers write on a client of their own, so a frame that the reader did
        # not read yet holds no part of the window of their session.
        self.writes = self.node.connect()
        self.addCleanup(self.writes.close)

    def read(self, reader: foundation.Reader, count: int) -> list:
        """The next `count` frames of `reader`, read on a thread that ends after
        `PATIENCE` s."""
        frames = []

        def read() -> None:
            for frame in reader:
                frames.append(frame)
                if len(frames) == count:
                    return

        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        thread.join(PATIENCE)
        self.assertFalse(thread.is_alive(), f"{len(frames)} of {count} frames came")
        return frames

    def test_a_reader_gets_each_frame_that_a_writer_of_another_client_writes(
        self,
    ) -> None:
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        os.chdir(self.node.dir.name)
        with foundation.connect(self.node.address) as client:
            reader = client.open_reader("plant.a")
            with (foundation.connect(self.node.address) as writes,
                  writes.open_writer("plant.a") as writer):
                for value in (1.5, 2.5, 3.5):
                    writer.write(value)
            frames = self.read(reader, 3)
            self.assertEqual([frame["plant.a"].tolist() for frame in frames],
                             [[1.5], [2.5], [3.5]])
            stamps = [frame["plant.time"][0] for frame in frames]
            self.assertEqual(stamps, sorted(stamps))
            for frame in frames:
                self.assertEqual(list(frame), ["plant.a", "plant.time"])
                self.assertEqual(frame.count, 1)
                self.assertEqual(frame.lost, {})
            self.assertIsNone(reader.close())

    def test_a_reader_gives_its_places_and_a_name_given_twice_once(self) -> None:
        channels = ["plant.b", "plant.a", "plant.b"]
        reader = called(self, self.client.open_reader, channels)
        self.addCleanup(reader.close)
        self.assertEqual(
            reader.places,
            (("plant.b", "i64", 3), ("plant.a", "f64", 3), ("plant.b", "i64", 3),
             ("plant.time", "timestamp", 3)),
        )
        with self.writes.open_writer(["plant.a", "plant.b"]) as writer:
            writer.write({"plant.a": 1.5, "plant.b": 7})
            writer.write({"plant.a": 2.5})
        both, one = self.read(reader, 2)
        self.assertIsInstance(both, collections.abc.Mapping)
        self.assertEqual(list(both), ["plant.b", "plant.a", "plant.time"])
        self.assertEqual(called(self, both.__len__), 3)
        self.assertEqual(both["plant.b"].tolist(), [7])
        keys = called(self, both.keys)
        self.assertEqual(list(keys), ["plant.b", "plant.a", "plant.time"])
        values = [series.tolist() for series in called(self, both.values)]
        self.assertEqual(values[:2], [[7], [1.5]])
        self.assertEqual([name for name, _ in called(self, both.items)], list(both))
        self.assertEqual(list(called(self, both.__iter__)), list(both))
        self.assertEqual(list(one), ["plant.a", "plant.time"])
        self.assertNotIn("plant.b", one)
        self.assertTrue(called(self, one.__contains__, "plant.a"))
        self.assertIsNone(called(self, one.get, "plant.b"))
        self.assertEqual(called(self, one.get, "plant.b", 5), 5)
        self.assertEqual(called(self, one.get, "plant.a").tolist(), [2.5])
        self.assertEqual(called(self, one.__getitem__, "plant.a").tolist(), [2.5])
        with self.assertRaises(KeyError) as raised:
            one["plant.b"]
        self.assertEqual(raised.exception.args, ("plant.b",))

    def test_a_latest_reader_gets_the_last_frame_and_loses_nothing(self) -> None:
        reader = self.client.open_reader("plant.a", mode="latest")
        self.addCleanup(reader.close)
        with self.writes.open_writer("plant.a") as writer:
            for value in range(50):
                writer.write(float(value))
        values = []
        frames = []

        def read() -> None:
            for frame in reader:
                frames.append(frame)
                values.append(frame["plant.a"][-1])
                if values[-1] == 49.0:
                    return

        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        thread.join(PATIENCE)
        self.assertFalse(thread.is_alive(), f"the last value came not: {values}")
        self.assertEqual(values, sorted(set(values)))
        self.assertEqual([frame.lost for frame in frames], [{}] * len(frames))

    def test_a_reader_gives_each_series_of_write_series_with_its_dtype(self) -> None:
        names = [
            "plant.a", "plant.b", "plant.c", "plant.d", "plant.f", "plant.g",
            "plant.i8", "plant.i16", "plant.i32", "plant.u16", "plant.u32",
            "plant.u64", "plant.timestamp",
        ]
        count = 6
        stamps = Stamps()
        first = stamps.take(count)
        series = {
            "plant.a": np.linspace(-1.0, 1.0, count),
            "plant.b": np.arange(count, dtype=np.int64) - (1 << 62),
            "plant.c": np.arange(250, 256, dtype=np.uint8),
            # A byte of 2 is True, as numpy reads it.
            "plant.d": np.array([0, 2, 1, 0, 255, 1], np.uint8).view(bool),
            "plant.f": np.array([0.5, -0.25, 3e38, 1e-38, 0, 1], np.float32),
            "plant.g": (np.arange(count, dtype=np.int64) * -7).astype(
                "timedelta64[ns]"
            ),
            "plant.i8": np.array([-128, -1, 0, 1, 2, 127], np.int8),
            "plant.i16": np.array([-32768, -1, 0, 1, 2, 32767], np.int16),
            "plant.i32": np.array([-(1 << 31), -1, 0, 1, 2, (1 << 31) - 1], np.int32),
            "plant.u16": np.array([0, 1, 2, 3, 4, 65535], np.uint16),
            "plant.u32": np.array([0, 1, 2, 3, 4, (1 << 32) - 1], np.uint32),
            "plant.u64": np.array([0, 1, 2, 3, 4, (1 << 64) - 1], np.uint64),
            "plant.timestamp": np.arange(count, dtype=np.int64).astype(
                "datetime64[ns]"
            ),
            "plant.time": first,
        }
        unaligned = np.frombuffer(bytes(1) + series["plant.a"].tobytes(), np.float64,
                                  offset=1)
        self.assertFalse(unaligned.flags.aligned)
        packed = np.zeros(count, [("a", "u1"), ("b", "<i8")])
        packed["b"] = series["plant.b"]
        second = stamps.take(count)
        dtypes = {
            "plant.a": "float64", "plant.b": "int64", "plant.c": "uint8",
            "plant.d": "bool", "plant.f": "float32", "plant.g": "timedelta64[ns]",
            "plant.i8": "int8", "plant.i16": "int16", "plant.i32": "int32",
            "plant.u16": "uint16", "plant.u32": "uint32", "plant.u64": "uint64",
            "plant.timestamp": "datetime64[ns]", "plant.time": "datetime64[ns]",
        }
        reader = self.client.open_reader(names)
        self.addCleanup(reader.close)
        with self.writes.open_writer(names) as writer:
            writer.write_series(series)
            writer.write_series({
                "plant.a": unaligned, "plant.b": packed["b"], "plant.time": second
            })
        whole, parts = self.read(reader, 2)
        self.assertEqual(list(whole), names + ["plant.time"])
        self.assertEqual(whole.count, count)
        for name, array in whole.items():
            self.assertEqual(array.dtype, np.dtype(dtypes[name]), name)
            self.assertTrue(array.dtype.isnative, name)
            self.assertEqual(array.shape, (count,), name)
            self.assertTrue(array.flags.writeable, name)
            given = series[name]
            if given.dtype != array.dtype:
                given = given.astype(dtypes[name])
            np.testing.assert_array_equal(array, given, name)
        self.assertEqual(
            whole["plant.d"].tolist(), [False, True, True, False, True, True]
        )
        self.assertEqual(list(parts), ["plant.a", "plant.b", "plant.time"])
        np.testing.assert_array_equal(parts["plant.a"], series["plant.a"])
        np.testing.assert_array_equal(parts["plant.b"], series["plant.b"])
        np.testing.assert_array_equal(parts["plant.time"], second.astype(
            "datetime64[ns]"
        ))

    def test_a_reader_gives_each_uuid_of_write_as_a_uuid(self) -> None:
        keys = [uuid.UUID(int=0), uuid.UUID(int=(1 << 128) - 1), uuid.uuid4()]
        reader = self.client.open_reader(["plant.e", "plant.a"])
        self.addCleanup(reader.close)
        with self.writes.open_writer(["plant.e", "plant.a"]) as writer:
            for n, key in enumerate(keys):
                writer.write({"plant.e": key, "plant.a": n + 0.5})
        frames = self.read(reader, len(keys))
        for n, (frame, key) in enumerate(zip(frames, keys)):
            self.assertEqual(frame["plant.e"].dtype, np.dtype(object))
            self.assertEqual(frame["plant.e"].shape, (1,))
            self.assertIs(type(frame["plant.e"][0]), uuid.UUID)
            self.assertEqual(frame["plant.e"][0], key)
            self.assertEqual(frame["plant.a"].tolist(), [n + 0.5])

    def test_an_array_of_a_frame_keeps_its_values_after_the_next_frame(self) -> None:
        reader = self.client.open_reader("plant.a")
        self.addCleanup(reader.close)
        stamps = Stamps()
        with self.writes.open_writer("plant.a") as writer:
            for value in (1.0, 2.0):
                writer.write_series({
                    "plant.a": np.full(4, value), "plant.time": stamps.take(4)
                })
        first = called(self, reader.__next__)["plant.a"]
        second = next(reader)["plant.a"]
        self.assertEqual(first.tolist(), [1.0] * 4)
        self.assertEqual(second.tolist(), [2.0] * 4)

    def lost(self, on_loss: str) -> tuple:
        """A reader of `plant.a` with `on_loss`, the frame after a loss that it got, or
        what it raised, and each record of the `foundation` logger in that run, as
        `assertLogs` gives it. The reader reads on a thread on a new node, while a
        writer on another client writes frames of `COUNT` samples until the node finds
        no room for one, then frames of one sample of 1.5 until one fits, then one of
        2.5, which the reader has not read yet."""
        logger = logging.getLogger("foundation")
        logged = logging.handlers.BufferingHandler(sys.maxsize)
        logger.addHandler(logged)
        self.addCleanup(logger.removeHandler, logged)
        # The node does not trim its buffer yet (#160), so a full ring has room only at
        # its end, and how the node batched the frames sets that room. No two frames of
        # one sample fit in some runs, so the input starts again on a new node.
        for _ in range(10):
            logged.buffer.clear()
            node = Node()
            self.addCleanup(node.stop)
            client = planted(node)
            self.addCleanup(client.close)
            reader = client.open_reader("plant.a", on_loss=on_loss)
            self.addCleanup(reader.close)
            got = []

            def read() -> None:
                try:
                    for frame in reader:
                        if frame.lost:
                            got.append(frame)
                            return
                except BaseException as error:
                    got.append(error)

            thread = threading.Thread(target=read, daemon=True)
            thread.start()
            with node.connect() as writes:
                fit = self.flooded(writes)
                if fit == 2:
                    thread.join(PATIENCE)
                    self.assertEqual(
                        len(got), 1, f"no frame after a loss in {PATIENCE} s"
                    )
                    records = [
                        f"{r.levelname}:{r.name}:{r.getMessage()}"
                        for r in logged.buffer
                    ]
                    return reader, got[0], records
            reader.close()
            thread.join(PATIENCE)
            if fit == 0:
                self.assertEqual(got, [])
            else:
                # The close can come before or after the read of the frame of 1.5.
                self.assertLessEqual(len(got), 1, got)
                kind = foundation.DataLost if on_loss == "raise" else foundation.Frame
                for item in got:
                    self.assertIsInstance(item, kind)
        self.fail("in 10 runs, no two frames of one sample found room after a loss")

    def flooded(self, client: foundation.Client) -> int:
        """Writes frames of `COUNT` samples of `plant.a` on `client` until the node
        finds no room for one, then frames of one sample of 1.5 until one fits, then one
        of 2.5. Gives how many of a frame of 1.5 and the frame of 2.5 fit: 0, 1, or
        2."""
        random = np.random.default_rng(3321)
        stamps = Stamps()
        with client.open_writer("plant.a", on_loss="raise") as writer:
            for _ in range(2000):
                try:
                    floats = random.random(COUNT)
                    writer.write_series(
                        {"plant.a": floats, "plant.time": stamps.take(COUNT)}
                    )
                except foundation.DataLost:
                    break
            else:
                self.fail(f"2000 frames of {COUNT} samples lost no sample")
            for _ in range(10):
                try:
                    writer.write(1.5)
                    break
                except foundation.DataLost:
                    pass
            else:
                return 0
            try:
                writer.write(2.5)
            except foundation.DataLost:
                return 1
        return 2

    def assert_lost(self, lost: dict) -> str:
        """Asserts that `lost` holds one range of seqs of `plant.a` and its index, and
        gives the text of the loss."""
        self.assertEqual(list(lost), ["plant.a", "plant.time"])
        self.assertIsInstance(lost["plant.a"], range)
        self.assertEqual(lost["plant.a"], lost["plant.time"])
        self.assertGreater(len(lost["plant.a"]), 0)
        count = len(lost["plant.a"])
        return f"the reader lost seqs: plant.a ({count}), plant.time ({count})"

    def assert_read_on(self, reader: foundation.Reader) -> None:
        """Asserts that the next frame of `reader` after its frame after a loss holds
        the sample of 2.5 that `flooded` wrote last, with no loss."""
        frame = next(reader)
        self.assertEqual(frame["plant.a"].tolist(), [2.5])
        self.assertEqual(frame.lost, {})

    def test_a_frame_after_a_loss_raises_data_lost_and_closes_the_reader(
        self,
    ) -> None:
        reader, raised, _ = self.lost("raise")
        self.assertIsInstance(raised, foundation.DataLost)
        assert_stated(self, raised)
        frame = raised.frame
        self.assertIsInstance(frame, foundation.Frame)
        text = self.assert_lost(frame.lost)
        self.assertEqual(raised.seqs, frame.lost)
        self.assertEqual(str(raised), text)
        self.assertEqual(frame["plant.a"].shape, (frame.count,))
        with self.assertRaises(StopIteration):
            next(reader)

    def test_a_frame_after_a_loss_warns_with_on_loss_warn(self) -> None:
        reader, frame, logged = self.lost("warn")
        self.assertIsInstance(frame, foundation.Frame)
        text = self.assert_lost(frame.lost)
        self.assertEqual(logged, [f"WARNING:foundation:{text}"])
        self.assert_read_on(reader)

    def test_a_frame_after_a_loss_passes_with_on_loss_ignore(self) -> None:
        with self.assertNoLogs("foundation"):
            reader, frame, _ = self.lost("ignore")
        self.assertIsInstance(frame, foundation.Frame)
        self.assert_lost(frame.lost)
        self.assert_read_on(reader)

    def test_open_reader_raises_for_each_argument_that_it_does_not_take(self) -> None:
        cases = [
            (lambda: self.client.open_reader("plant.a", mode="all"), ValueError,
             'mode is "all", not "complete" or "latest"'),
            (lambda: self.client.open_reader("plant.a", on_loss="log"), ValueError,
             'on_loss is "log", not "warn", "raise", or "ignore"'),
            (lambda: self.client.open_reader([]), ValueError,
             "the stream names no channel"),
            (lambda: self.client.open_reader(7), TypeError,
             "'int' object is not iterable"),
            (lambda: self.client.open_reader([7]), TypeError,
             "a channel name is a str, not int"),
            (lambda: self.client.open_reader("plant.string"), TypeError,
             "plant.string holds string, which the reader gives no series of"),
        ]
        for call, kind, text in cases:
            with self.assertRaises(kind) as raised:
                call()
            self.assertEqual(str(raised.exception), text)
        self.assertEqual(
            self.client.open_reader("plant.a").places,
            (("plant.a", "f64", 1), ("plant.time", "timestamp", 1)),
        )

    def test_open_reader_raises_stopped_with_the_code_of_the_node(self) -> None:
        for names in (["plant.a", "plant.z"], ["plant.z"]):
            with self.assertRaises(foundation.Stopped) as raised:
                self.client.open_reader(names)
            self.assertEqual(raised.exception.code, "unknown")
            self.assertEqual(
                str(raised.exception), f"{UNKNOWN}: the open names {', '.join(names)}"
            )
        with self.assertRaises(foundation.Stopped) as raised:
            self.client.open_reader(["plant.a", "other.a"])
        self.assertEqual(raised.exception.code, "malformed")

    def test_a_reader_raises_connection_error_after_the_node_stopped(self) -> None:
        reader = self.client.open_reader("plant.a")
        self.node.stop()
        text = "the session failed: the peer closed the session (0)"
        for call in (lambda: next(reader), lambda: next(reader), self.client.close,
                     self.writes.close):
            with self.assertRaises(ConnectionError) as raised:
                call()
            self.assertEqual(str(raised.exception), text)
        self.assertIsNone(reader.close())

    def test_a_reader_of_a_closed_client_raises(self) -> None:
        reader = self.client.open_reader("plant.a")
        self.client.close()
        for _ in range(2):
            with self.assertRaises(RuntimeError) as raised:
                next(reader)
            self.assertEqual(str(raised.exception), "the client is closed")
        with self.assertRaises(RuntimeError) as raised:
            self.client.open_reader("plant.a")
        self.assertEqual(str(raised.exception), "the client is closed")

    def test_other_threads_run_while_a_reader_waits(self) -> None:
        reader = self.client.open_reader("plant.a")
        self.addCleanup(reader.close)
        counts = []

        def count_then_write() -> None:
            deadline = time.monotonic() + 0.2
            count = 0
            while time.monotonic() < deadline:
                count += 1
                time.sleep(0)
            counts.append(count)
            with self.writes.open_writer("plant.a") as writer:
                writer.write(1.5)

        thread = threading.Thread(target=count_then_write, daemon=True)
        thread.start()
        self.assertEqual(next(reader)["plant.a"].tolist(), [1.5])
        thread.join(PATIENCE)
        self.assertGreater(counts[0], 20)

    def test_ctrl_c_ends_a_reader_that_waits_and_the_next_call_gets_the_frame(
        self,
    ) -> None:
        # CPython runs the signal handlers only on the thread that started it, which in
        # `cargo test` can be another test's, and only there does `set_wakeup_fd` not
        # raise. `threading.main_thread` and `signal.getsignal` do not tell. The wheel
        # jobs run this test.
        try:
            signal.set_wakeup_fd(signal.set_wakeup_fd(-1))
        except ValueError:
            self.skipTest("Python runs no signal handler on this thread")
        reader = self.client.open_reader("plant.a")
        self.addCleanup(reader.close)
        interrupted = []
        ended = threading.Event()

        def interrupt() -> None:
            time.sleep(0.2)
            interrupted.append(time.monotonic())
            _thread.interrupt_main()
            # A reader that Ctrl-C does not end fails the test, and does not hang it.
            if not ended.wait(PATIENCE):
                reader.close()

        thread = threading.Thread(target=interrupt, daemon=True)
        thread.start()
        with self.assertRaises(KeyboardInterrupt):
            for _ in reader:
                pass
        ended.set()
        self.assertLess(time.monotonic() - interrupted[0], 1.0)
        thread.join(PATIENCE)
        with self.writes.open_writer("plant.a") as writer:
            writer.write(1.5)
        self.assertEqual(next(reader)["plant.a"].tolist(), [1.5])

    def test_a_close_on_another_thread_ends_a_reader_that_waits(self) -> None:
        reader = self.client.open_reader("plant.a")
        closed = []

        def close() -> None:
            time.sleep(0.2)
            closed.append(time.monotonic())
            reader.close()

        thread = threading.Thread(target=close, daemon=True)
        thread.start()
        self.assertEqual(list(reader), [])
        self.assertLess(time.monotonic() - closed[0], 1.0)
        thread.join(PATIENCE)
        self.assertIsNone(reader.close())

    def test_a_with_block_closes_the_reader(self) -> None:
        with self.client.open_reader("plant.a") as reader:
            pass
        with self.assertRaises(StopIteration):
            next(reader)
        reader = called(self, self.client.open_reader, "plant.a", mode="latest",
                        on_loss="raise")
        self.assertIs(called(self, reader.__enter__), reader)
        self.assertIs(called(self, reader.__iter__), reader)
        called(self, reader.__exit__, None, None, None)
        called(self, reader.close)
        with self.assertRaises(ValueError):
            with self.client.open_reader("plant.a") as reader:
                raise ValueError("the block")
        self.assertEqual(list(reader), [])

    def test_a_forked_child_does_not_own_the_reader(self) -> None:
        reader = self.client.open_reader("plant.a")
        self.addCleanup(reader.close)
        parent = os.getpid()

        def child() -> object:
            text = (
                f"the client connected in process {parent}, and this is process "
                f"{os.getpid()}: connect again in this process"
            )
            calls = [lambda: next(reader), lambda: self.client.open_reader("plant.a")]
            for call in calls:
                try:
                    call()
                except RuntimeError as error:
                    if str(error) != text:
                        return str(error)
                else:
                    return "the child read or opened"
            return reader.close()

        self.assertEqual(fork(child), 0)
        with self.writes.open_writer("plant.a") as writer:
            writer.write(1.5)
        self.assertEqual(next(reader)["plant.a"].tolist(), [1.5])

    def test_a_child_that_drops_an_open_reader_touches_no_descriptor(self) -> None:
        # The client's thread opens the second reader only after the task of the first
        # waits for a call, so a drop of the first that is not forgotten wakes it.
        readers = [self.client.open_reader("plant.a") for _ in range(2)]

        def drop() -> object:
            raised = []
            sys.unraisablehook = raised.append
            os.closerange(3, 65536)
            readers.clear()
            gc.collect()
            return raised or None

        self.assertEqual(fork(drop), 0)
        self.assertEqual([reader.close() for reader in readers], [None, None])

    def test_a_close_from_the_garbage_collector_during_next_does_not_hang(
        self,
    ) -> None:
        reader = self.client.open_reader("plant.a")
        with self.writes.open_writer("plant.a") as writer:
            writer.write(1.5)
        closed = []

        class Cycle:
            def __del__(self) -> None:
                reader.close()
                closed.append(True)

        threshold = gc.get_threshold()
        self.addCleanup(gc.set_threshold, *threshold)
        self.addCleanup(gc.enable)
        gc.disable()
        cycle = Cycle()
        cycle.cycle = cycle
        del cycle
        gc.set_threshold(1)
        got = []

        def read() -> None:
            # The first object that `__next__` makes runs the collector.
            gc.enable()
            got.append(next(reader))
            got.append(next(reader, "stopped"))

        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        thread.join(10)
        self.assertFalse(thread.is_alive(), "a close from __del__ hangs")
        self.assertEqual(closed, [True])
        self.assertEqual(got[0]["plant.a"].tolist(), [1.5])
        self.assertEqual(got[1], "stopped")

    def test_a_reader_of_each_index_reads_the_frames_of_a_writer_of_two(self) -> None:
        plant = self.client.open_reader(["plant.a", "plant.b"])
        self.addCleanup(plant.close)
        other = self.client.open_reader("other.a")
        self.addCleanup(other.close)
        with self.writes.open_writer(["plant.a", "other.a", "plant.b"]) as writer:
            writer.write({"plant.a": 1.5, "other.a": 2.5, "plant.b": 3})
            writer.write({"other.a": 6.5})
            plant_stamps, other_stamps = Stamps(), Stamps()
            # Ahead of the node's stamps of the frames before.
            plant_stamps.next += 10**9
            other_stamps.next += 10**9
            stamps = (int(plant_stamps.take(1)[0]), int(other_stamps.take(1)[0]))
            writer.write({
                "plant.a": 4.5,
                "plant.b": 5,
                "other.a": 7.5,
                "plant.time": stamps[0],
                "other.time": stamps[1],
            })
            writer.write_series({
                "plant.a": np.array([8.5, 9.5]),
                "plant.b": np.array([10, 11], dtype=np.int64),
                "plant.time": plant_stamps.take(2),
                "other.a": np.array([12.5, 13.5, 14.5]),
                "other.time": other_stamps.take(3),
            })
        plants = self.read(plant, 3)
        self.assertEqual(
            [(f["plant.a"].tolist(), f["plant.b"].tolist()) for f in plants],
            [([1.5], [3]), ([4.5], [5]), ([8.5, 9.5], [10, 11])],
        )
        self.assertEqual(plants[1]["plant.time"].astype(np.int64)[0], stamps[0])
        others = self.read(other, 4)
        self.assertEqual(
            [f["other.a"].tolist() for f in others],
            [[2.5], [6.5], [7.5], [12.5, 13.5, 14.5]],
        )
        self.assertEqual(others[2]["other.time"].astype(np.int64)[0], stamps[1])


if __name__ == "__main__":
    unittest.main()
