"""The test-only functions of a build with the feature `sim`."""

import unittest

from foundation import _native


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


if __name__ == "__main__":
    unittest.main()
