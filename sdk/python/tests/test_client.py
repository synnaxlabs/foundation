"""The Python SDK against a node process.

`FOUNDATION_NODE` is the path of the `foundation` binary that each test starts.
"""

import gc
import os
import pathlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import typing
import unittest
import warnings

import foundation
from test_stubs import assert_stated, called

PATIENCE = 90.0
# The node sends its close before it exits. No call of the client shows that the client
# took it, so a test gives the client 1 s on loopback. It goes with item 1 of #2953.
CLOSED = 1.0
ADMIN = "@admin"
# The encoding of the base point of Ed25519: a public key that is not of small order.
BASE = bytes([0x58]) + bytes([0x66] * 31)
WRONG = bytes(31) + b"\x01"
# The watcher of a node, `sh -c WATCH <pid> <directory>`. Its stdin ends when the test
# process and its forked children end.
WATCH = """
cat >/dev/null
kill -CONT "$0"; kill -TERM "$0"
while kill -0 "$0" 2>/dev/null; do sleep 0.1; done
rm -rf "$1"
"""
FORKED = (
    "a connect in a forked child traps on macOS (#3370): connect in the test process"
)


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 & -(crc & 1))
    return crc ^ 0xFFFFFFFF


def budget(pool: int, disk: int) -> bytes:
    """The budget file of a node: the tag, `pool` and `disk`, and the CRC32C."""
    body = b"foundation/budget/1" + pool.to_bytes(8, "little")
    body += disk.to_bytes(8, "little")
    return body + crc32c(body).to_bytes(4, "little")


def forked(*_, **__) -> typing.NoReturn:
    """Takes the place of `connect` in the child of `spawn`, and raises
    `AssertionError`."""
    raise AssertionError(FORKED)


def fork(call) -> int:
    """Calls `call` in a forked child, and gives the exit code of the child: 0 when
    `call` gives `None`. Kills the child after `PATIENCE` s."""
    return exit_code(spawn(call))


def spawn(call) -> int:
    """Calls `call` in a forked child, and gives the pid of the child. The child exits
    with 0 when `call` gives `None`, else with 1. In the child, `foundation.connect` and
    `foundation._native.connect` raise `AssertionError` on each OS."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", DeprecationWarning)
        pid = os.fork()
    if pid == 0:
        code = 1
        try:
            foundation.connect = sys.modules["foundation._native"].connect = forked
            code = 0 if call() is None else 1
        finally:
            os._exit(code)
    return pid


def exit_code(pid: int) -> int:
    """Waits for the child `pid`, and gives its exit code. Kills it after `PATIENCE`
    s."""
    deadline = time.monotonic() + PATIENCE
    while True:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            return os.waitstatus_to_exitcode(status)
        if time.monotonic() > deadline:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            raise AssertionError(f"the child {pid} did not exit")
        time.sleep(0.01)


def close(client: foundation.Client, entered: threading.Event, ends: list) -> None:
    """Sets `entered`, then closes `client`, and adds to `ends` what the close gives or
    raises."""
    entered.set()
    try:
        ends.append(client.close())
    except BaseException as error:
        ends.append(error)


def quick(call) -> object:
    """Calls `call`, and gives what it gives when it returns within 1 s, else
    `"slow"`."""
    start = time.monotonic()
    given = call()
    return given if time.monotonic() - start < 1 else "slow"


class Node:
    """`foundation start --name edge` in a new directory, with a budget of 1 GiB of
    pool and 8 MiB of disk for each core. It goes with #3064. A watcher stops the node
    and removes the directory when the test process dies before `stop`."""

    def __init__(self) -> None:
        self.start()
        self.address = self.first().address

    def start(self) -> None:
        """Makes the directory, starts the node and its watcher, and waits for the
        first line of the node."""
        self.dir = tempfile.TemporaryDirectory(prefix="foundation-python-")
        self.data = os.path.join(self.dir.name, "foundation-data")
        os.makedirs(os.path.join(self.data, "data"))
        with open(os.path.join(self.data, "data", "budget"), "wb") as file:
            file.write(budget(1 << 30, os.cpu_count() * (8 << 20)))
        self.process = subprocess.Popen(
            [os.environ["FOUNDATION_NODE"], "start", "--name", "edge"],
            cwd=self.dir.name,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.watcher = subprocess.Popen(
            ["sh", "-c", WATCH, str(self.process.pid), self.dir.name],
            stdin=subprocess.PIPE,
            start_new_session=True,
        )
        line = self.process.stdout.readline()
        if not line:
            self.process.wait(PATIENCE)
            raise AssertionError(
                f"the node exited with {self.process.returncode}:\n"
                + self.process.stderr.read().decode()
            )

    def first(self) -> foundation.Client:
        """A closed client of the first connect from the data directory. The node
        prints its line before it writes the file `address`."""
        deadline = time.monotonic() + PATIENCE
        while True:
            try:
                client = foundation.connect(data=self.data)
            except ConnectionError as error:
                if not str(error).startswith("no node runs in "):
                    raise
                if time.monotonic() > deadline:
                    raise AssertionError(f"the node writes no address: {error}")
                time.sleep(0.01)
                continue
            client.close()
            return client

    def connect(self, **arguments) -> foundation.Client:
        """`foundation.connect` to this node as its admin, from its data directory,
        with `arguments`."""
        return foundation.connect(**{"data": self.data, **arguments})

    def pause(self) -> None:
        """Stops the node with SIGSTOP, and returns once it stopped: on macOS, it can
        run on after `kill` returns. `resume` and `stop` continue it."""
        self.process.send_signal(signal.SIGSTOP)
        os.waitpid(self.process.pid, os.WUNTRACED)

    def resume(self) -> None:
        """Continues the node after `pause`."""
        self.process.send_signal(signal.SIGCONT)

    def stop(self) -> None:
        """Stops the watcher, then the node with SIGTERM, waits until the node exits,
        and removes the directory."""
        self.watcher.kill()
        self.watcher.communicate()
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGCONT)
            self.process.send_signal(signal.SIGTERM)
        self.process.communicate(timeout=PATIENCE)
        self.dir.cleanup()


class ClientTest(unittest.TestCase):
    def setUp(self) -> None:
        self.node = Node()
        self.addCleanup(self.node.stop)

    def test_a_client_connects_and_connects_again_after_its_close(self) -> None:
        client = self.node.connect()
        called(self, client.close)
        self.assertIsNone(client.close())
        client = self.node.connect()
        del client

    def test_a_with_block_closes_the_client(self) -> None:
        with self.node.connect() as client:
            self.assertIsInstance(client, foundation.Client)
        client = self.node.connect()
        self.assertIs(called(self, client.__enter__), client)
        called(self, client.__exit__, None, None, None)
        self.node.stop()
        time.sleep(CLOSED)
        self.assertIsNone(client.close())

    def test_a_raised_block_gets_no_note_when_the_close_succeeds(self) -> None:
        with self.assertRaises(KeyError) as raised:
            with self.node.connect():
                raise KeyError("in the block")
        self.assertFalse(hasattr(raised.exception, "__notes__"))

    def test_two_threads_that_close_one_client_both_return(self) -> None:
        client = self.node.connect()
        threads = [
            threading.Thread(target=client.close, daemon=True) for _ in range(2)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(PATIENCE)
        self.assertEqual([thread.is_alive() for thread in threads], [False, False])

    def test_other_threads_run_while_a_client_closes(self) -> None:
        client = self.node.connect()
        self.node.pause()
        count = 0
        closed = threading.Event()

        def counter() -> None:
            nonlocal count
            while not closed.is_set():
                count += 1
                # Gives up the GIL with no wait: on macOS a sleep of 1 ms can take
                # 20 ms, and the close of a paused node is short (about 0.1 s on
                # x86_64 Linux).
                time.sleep(0)

        thread = threading.Thread(target=counter, daemon=True)
        thread.start()
        try:
            client.close()
        finally:
            closed.set()
            thread.join()
        self.assertGreater(count, 20)

    def test_a_child_forked_during_a_close_gives_none(self) -> None:
        deadline = time.monotonic() + PATIENCE
        late = True
        while True:
            client = self.node.connect()
            self.node.pause()
            entered = threading.Event()
            ends: list = []
            closing = threading.Thread(
                target=close, args=(client, entered, ends), daemon=True
            )
            closing.start()
            self.assertTrue(entered.wait(PATIENCE))
            if late:
                # The first try forks after the close, as on a slow runner, so each run
                # takes the path that tries again.
                closing.join(PATIENCE)
            during = False
            while True:
                # A close in a child does no work, so only a wait on the lock that the
                # fork copied as held makes it last 1 s.
                child = spawn(lambda: quick(client.close))
                # The close can hold the lock of the client only before it returns.
                alive = not ends
                self.assertEqual(exit_code(child), 0)
                if not alive:
                    break
                during = True
                self.assertLess(time.monotonic(), deadline, "the close did not end")
            closing.join(PATIENCE)
            self.assertEqual(ends, [None])
            if during:
                return
            # The close of a paused node is short (about 0.1 s on x86_64 Linux), so on
            # a slow runner the first fork can come after it. Such a try proves nothing.
            self.assertLess(time.monotonic(), deadline, "each fork came after a close")
            self.node.resume()
            late = False

    def test_a_child_forked_with_an_open_client_does_not_end_the_session(self) -> None:
        clients = [self.node.connect()]

        def close_and_drop() -> object:
            raised = []
            sys.unraisablehook = raised.append
            closed = clients[0].close()
            clients.clear()
            gc.collect()
            return raised or closed

        self.assertEqual(fork(close_and_drop), 0)
        self.assertIsNone(clients[0].close())

    def test_a_child_that_drops_an_open_client_touches_no_descriptor(self) -> None:
        clients = [self.node.connect()]

        def drop() -> object:
            raised = []
            sys.unraisablehook = raised.append
            os.closerange(3, 65536)
            clients.clear()
            gc.collect()
            return raised or None

        self.assertEqual(fork(drop), 0)
        self.assertIsNone(clients[0].close())

    def test_a_connect_with_no_argument_on_the_nodes_host_is_the_first_admin(
        self,
    ) -> None:
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        os.chdir(self.node.dir.name)
        with called(self, foundation.connect) as client:
            self.assertEqual(client.address, self.node.address)
        with called(self, foundation.connect, self.node.address) as client:
            self.assertEqual(client.address, self.node.address)

    def test_the_subject_keywords_win_over_the_data_directory(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            self.node.connect(subject=ADMIN, private_key=bytes(32))
        assert_stated(self, raised.exception)
        self.assertEqual(raised.exception.code, "refused")
        self.assertEqual(
            str(raised.exception),
            "the spec has no such subject, does not list the key for it, or the "
            "signature is not valid",
        )

    def test_the_address_keyword_wins_over_the_data_directory(self) -> None:
        at = f"[::1]:{self.node.address.rsplit(':', 1)[1]}"
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(at, data=self.node.data)
        self.assertEqual(
            str(raised.exception),
            f"no source gives the public key of the node at {at}: {self.node.data} "
            f"holds the node at {self.node.address}; pass public_key=, or connect to "
            f"{self.node.address}",
        )
        with self.assertRaises(ConnectionError) as raised:
            called(
                self,
                foundation.connect,
                at,
                public_key=WRONG,
                data=self.node.data,
            )
        self.assertEqual(
            str(raised.exception),
            f"no source gives the subject for the node at {at}: {self.node.data} "
            f"holds the node at {self.node.address}; pass subject= and private_key=, "
            f"or connect to {self.node.address}",
        )

    def test_the_data_directory_can_be_a_path(self) -> None:
        data = pathlib.Path(self.node.data)
        with called(self, foundation.connect, data=data) as client:
            self.assertEqual(client.address, self.node.address)

    def test_a_data_directory_with_no_admin_key_gives_no_subject(self) -> None:
        copy = tempfile.TemporaryDirectory(prefix="foundation-python-")
        self.addCleanup(copy.cleanup)
        os.makedirs(os.path.join(copy.name, "data"))
        shutil.copyfile(
            os.path.join(self.node.data, "data", "address"),
            os.path.join(copy.name, "data", "address"),
        )
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(data=copy.name)
        self.assertEqual(
            str(raised.exception), f"{copy.name} holds no admin.key that a node wrote"
        )
        with self.assertRaises(ConnectionError) as raised:
            called(
                self,
                foundation.connect,
                data=copy.name,
                subject="plant.operator",
                private_key=bytes(32),
            )
        self.assertEqual(
            str(raised.exception),
            "the spec has no such subject, does not list the key for it, or the "
            "signature is not valid",
        )

    def test_a_subject_that_the_spec_does_not_list_is_refused(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            self.node.connect(subject="plant.operator", private_key=bytes(32))
        self.assertEqual(
            str(raised.exception),
            "the spec has no such subject, does not list the key for it, or the "
            "signature is not valid",
        )

    def test_a_wrong_public_key_is_not_authenticated(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            self.node.connect(public_key=WRONG)
        address = self.node.address
        self.assertEqual(
            str(raised.exception),
            f"the session failed: no address reached peer {WRONG.hex()}; "
            f"Udp({address}): the peer did not prove key {WRONG.hex()}",
        )

    def test_the_close_gives_the_cause_when_the_node_closed_the_session(self) -> None:
        client = self.node.connect()
        self.node.stop()
        time.sleep(CLOSED)
        with self.assertRaises(ConnectionError) as raised:
            client.close()
        self.assertEqual(
            str(raised.exception), "the session failed: the peer closed the session (0)"
        )
        self.assertIsNone(client.close())

    def test_a_with_block_raises_the_close_error_when_it_did_not_raise(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            with self.node.connect():
                self.node.stop()
                time.sleep(CLOSED)
        self.assertEqual(
            str(raised.exception), "the session failed: the peer closed the session (0)"
        )

    def test_a_with_block_that_raised_gives_the_close_error_as_a_note(self) -> None:
        with self.assertRaises(KeyError) as raised:
            with self.node.connect():
                self.node.stop()
                time.sleep(CLOSED)
                raise KeyError("in the block")
        self.assertEqual(repr(raised.exception), "KeyError('in the block')")
        self.assertEqual(
            raised.exception.__notes__,
            [
                "the close raised ConnectionError: the session failed: the peer closed "
                "the session (0)"
            ],
        )


def alive(pid: int) -> bool:
    """Whether a process `pid` runs."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class NodeTest(unittest.TestCase):
    def test_a_node_stops_and_its_directory_goes_when_its_test_process_is_killed(
        self,
    ) -> None:
        read, write = os.pipe()

        def started() -> None:
            node = Node.__new__(Node)
            node.start()
            os.write(write, f"{node.process.pid} {node.dir.name}\n".encode())
            time.sleep(PATIENCE)

        child = spawn(started)
        os.close(write)
        with os.fdopen(read) as lines:
            line = lines.readline()
        os.kill(child, signal.SIGKILL)
        os.waitpid(child, 0)
        self.assertNotEqual(line, "", "the child started no node")
        pid, directory = int(line.split()[0]), line.split()[1]
        deadline = time.monotonic() + PATIENCE
        while alive(pid) or os.path.exists(directory):
            if time.monotonic() > deadline:
                if alive(pid):
                    os.kill(pid, signal.SIGKILL)
                shutil.rmtree(directory, ignore_errors=True)
                self.fail(f"the node {pid} or its directory {directory} is left")
            time.sleep(0.01)


class ArgumentTest(unittest.TestCase):
    def connect(self, **arguments) -> foundation.Client:
        return foundation.connect(
            **{
                "address": "127.0.0.1:1",
                "public_key": BASE,
                "subject": ADMIN,
                "private_key": bytes(32),
                **arguments,
            }
        )

    def test_a_connect_in_a_forked_child_raises_assertion_error(self) -> None:
        def child() -> object:
            for connect in (self.connect, sys.modules["foundation._native"].connect):
                try:
                    connect()
                except AssertionError as error:
                    if str(error) != FORKED:
                        return str(error)
                else:
                    return "connected"
            return None

        self.assertEqual(fork(child), 0)

    def directory(self) -> str:
        """A new data directory, with its `data`, that holds no node."""
        dir = tempfile.TemporaryDirectory(prefix="foundation-python-")
        self.addCleanup(dir.cleanup)
        os.makedirs(os.path.join(dir.name, "data"))
        return dir.name

    def test_a_pair_given_in_half_is_refused(self) -> None:
        for half, text in [
            ({"subject": ADMIN}, "subject and private_key"),
            ({"private_key": bytes(32)}, "subject and private_key"),
        ]:
            with self.assertRaises(ValueError) as raised:
                foundation.connect("127.0.0.1:1", data=self.directory(), **half)
            self.assertEqual(
                str(raised.exception), f"{text} go together: give both or neither"
            )

    def test_a_connect_with_each_argument_reads_no_data_directory(self) -> None:
        data = self.directory()
        os.chmod(os.path.join(data, "data"), 0)
        self.addCleanup(os.chmod, os.path.join(data, "data"), 0o700)
        with self.assertRaises(ConnectionError) as raised:
            self.connect(address="[::1]:0", data=data)
        self.assertEqual(
            str(raised.exception),
            f"the session failed: no address reached peer {BASE.hex()}; "
            "Udp([::1]:0): this node cannot send to that address",
        )

    def test_no_address_and_no_node_in_the_data_directory_is_refused(self) -> None:
        data = self.directory()
        for dir in [data, os.path.join(data, "other")]:
            with self.assertRaises(ConnectionError) as raised:
                foundation.connect(data=dir)
            self.assertEqual(
                str(raised.exception),
                f"no node runs in {dir}: pass the address of the node, or data= with "
                "the --data of foundation start",
            )

    def test_no_source_of_the_node_keys_is_refused(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect("127.0.0.1:1", data=self.directory())
        self.assertEqual(
            str(raised.exception),
            "no source gives the public key of the node at 127.0.0.1:1: pass "
            "public_key=, or data= with the --data of foundation start on the node's "
            "host",
        )

    def test_no_source_of_the_subject_is_refused(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(
                "127.0.0.1:1",
                public_key=BASE,
                data=self.directory(),
            )
        self.assertEqual(
            str(raised.exception),
            "no source gives the subject for the node at 127.0.0.1:1: pass subject= "
            "and private_key=, or data= with the --data of foundation start on the "
            "node's host",
        )

    def test_each_argument_parses_before_the_data_directory_is_read(self) -> None:
        data = self.directory()
        os.chmod(os.path.join(data, "data"), 0)
        self.addCleanup(os.chmod, os.path.join(data, "data"), 0o700)
        for arguments, text in [
            (
                {"address": "localhost:1"},
                "the address \"localhost:1\" is not ip:port: invalid socket address "
                "syntax",
            ),
            (
                {"public_key": bytes(31)},
                "public_key is 31 bytes, not 32",
            ),
            (
                {"subject": "", "private_key": bytes(32)},
                "subject: a name or pattern is empty",
            ),
        ]:
            with self.assertRaises(ValueError) as raised:
                foundation.connect(data=data, **arguments)
            self.assertEqual(str(raised.exception), text)

    def test_a_data_directory_that_cannot_be_read_is_refused(self) -> None:
        data = self.directory()
        os.chmod(os.path.join(data, "data"), 0)
        self.addCleanup(os.chmod, os.path.join(data, "data"), 0o700)
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(data=data)
        self.assertEqual(
            str(raised.exception),
            f"cannot read {data}/data: open of . failed with OS error 13",
        )

    def test_a_file_in_the_data_directory_that_cannot_be_read_is_refused(
        self,
    ) -> None:
        data = self.directory()
        address = os.path.join(data, "data", "address")
        with open(address, "wb") as file:
            file.write(b"foundation/address/1")
        os.chmod(address, 0)
        self.addCleanup(os.chmod, address, 0o600)
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(data=data)
        self.assertEqual(
            str(raised.exception),
            f"cannot read {data}/data: open of address failed with OS error 13",
        )

    def test_a_file_of_another_format_version_is_refused(self) -> None:
        data = self.directory()
        with open(os.path.join(data, "data", "address"), "wb") as file:
            file.write(b"foundation/address/2")
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect("127.0.0.1:1", data=data)
        self.assertEqual(
            str(raised.exception),
            f"{data}/data/address holds format version 2, and this package reads "
            "version 1: install the package of the node's version",
        )

    def test_an_address_that_is_not_ip_and_port_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            self.connect(address="localhost:1")
        self.assertEqual(
            str(raised.exception),
            "the address \"localhost:1\" is not ip:port: invalid socket address syntax",
        )

    def test_a_public_key_that_is_not_32_bytes_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            self.connect(public_key=bytes(31))
        self.assertEqual(str(raised.exception), "public_key is 31 bytes, not 32")

    def test_a_public_key_of_small_order_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            self.connect(public_key=bytes(32))
        self.assertEqual(
            str(raised.exception),
            "public_key: the public key is a point of small order",
        )

    def test_a_private_key_that_is_not_32_bytes_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            self.connect(private_key=bytes(33))
        self.assertEqual(str(raised.exception), "private_key is 33 bytes, not 32")

    def test_a_node_key_is_no_argument(self) -> None:
        with self.assertRaises(TypeError) as raised:
            self.connect(node=1)
        self.assertEqual(
            str(raised.exception), "connect() got an unexpected keyword argument 'node'"
        )

    def test_a_subject_that_is_not_a_name_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            self.connect(subject="")
        self.assertEqual(str(raised.exception), "subject: a name or pattern is empty")

    def test_a_data_directory_with_a_nul_byte_is_refused(self) -> None:
        for given, connect in (("data", foundation.connect), ("each", self.connect)):
            with self.subTest(given=given):
                with self.assertRaises(ValueError) as raised:
                    connect(data="a\0b")
                self.assertEqual(
                    str(raised.exception), "data: the path holds a NUL byte"
                )

    def test_other_threads_run_while_a_client_dials(self) -> None:
        silent = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
        self.addCleanup(silent.close)
        silent.bind(("::1", 0))
        port = silent.getsockname()[1]
        count = 0
        dialing = threading.Event()

        def counter() -> None:
            nonlocal count
            while not dialing.is_set():
                count += 1
                time.sleep(0.001)

        thread = threading.Thread(target=counter, daemon=True)
        thread.start()
        try:
            with self.assertRaises(ConnectionError) as raised:
                self.connect(address=f"[::1]:{port}")
        finally:
            dialing.set()
            thread.join()
        self.assertGreater(count, 100)
        self.assertEqual(
            str(raised.exception),
            f"the session failed: no address reached peer {BASE.hex()}; "
            f"Udp([::1]:{port}): the peer stopped answering",
        )


if __name__ == "__main__":
    unittest.main()
