"""The test-only functions of a build with the feature `sim`."""

import sys
import time
import unittest

import numpy as np

import foundation
from foundation import _native
from test_client import Node, fork
from test_writer import planted


class SimTest(unittest.TestCase):
    def test_a_panic_on_the_callers_thread_raises_panic_exception(self) -> None:
        with self.assertRaises(BaseException) as raised:
            _native._panic()
        self.assertEqual(type(raised.exception).__name__, "PanicException")
        self.assertEqual(str(raised.exception), "the test panicked")

    def test_a_panic_of_the_clients_thread_raises_panic_exception(self) -> None:
        with self.assertRaises(BaseException) as raised:
            _native._error("panicked")
        self.assertEqual(type(raised.exception).__name__, "PanicException")
        self.assertEqual(str(raised.exception), "the client thread panicked")

    def test_a_thread_that_cannot_start_raises_runtime_error(self) -> None:
        with self.assertRaises(RuntimeError) as raised:
            _native._error("thread")
        self.assertEqual(
            str(raised.exception), "cannot start the client thread: the test"
        )

    def test_an_unknown_kind_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            _native._error("bind")
        self.assertEqual(str(raised.exception), 'no error "bind"')


class WriterSimTest(unittest.TestCase):
    def test_a_call_that_raises_keeps_the_buffers_for_the_next_call(self) -> None:
        node = Node()
        self.addCleanup(node.stop)
        client = planted(node)
        self.addCleanup(client.close)
        floats = np.zeros(1000)
        stamps = np.arange(1000, dtype=np.int64) + time.time_ns()
        with client.open_writer("plant.a") as writer:
            self.assertEqual((writer._kept, writer._part), (0, 0))
            writer.write_series({"plant.a": floats, "plant.time": stamps})
            kept = writer._kept
            self.assertGreaterEqual(kept, 16000)
            with self.assertRaises(TypeError):
                writer.write_series({"plant.a": floats, "plant.time": floats})
            self.assertEqual(writer._kept, kept)
            with self.assertRaises(TypeError):
                writer.write({"plant.a": "x"})
            self.assertEqual(writer._kept, kept)
            self.assertGreater(writer._part, 0)

    def test_series_over_the_bytes_of_one_frame_raise_before_their_copy(self) -> None:
        node = Node()
        self.addCleanup(node.stop)
        planted(node).close()
        data = node.data
        count = 2**31

        def child() -> object:
            client = foundation.connect(data=data)
            series = {
                "plant.a": np.broadcast_to(np.zeros(1), count),
                "plant.time": np.broadcast_to(np.zeros(1, np.int64), count),
            }
            with client.open_writer("plant.a") as writer:
                kept = writer._kept
                # 8 GiB of address space over the child's size, so a copy of the 16 GiB
                # of `plant.a` fails the child and not the host.
                if sys.platform == "linux":
                    import resource

                    with open("/proc/self/statm") as statm:
                        size = int(statm.read().split()[0]) * resource.getpagesize()
                    limit = (size + 2**33, size + 2**33)
                    resource.setrlimit(resource.RLIMIT_AS, limit)
                try:
                    writer.write_series(series)
                except ValueError as error:
                    text = (
                        "the series hold 2147483648 samples, 17179869184 bytes, over "
                        "the 57344 bytes of one frame"
                    )
                    if str(error) != text:
                        return str(error)
                    return None if writer._kept == kept else writer._kept
            return "no error"

        self.assertEqual(fork(child), 0)


if __name__ == "__main__":
    unittest.main()
