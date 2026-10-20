"""The Python SDK against a node process.

`FOUNDATION_NODE` is the path of the `foundation` binary that each test starts.
"""

import base64
import errno
import gc
import getpass
import json
import os
import pathlib
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import typing
import unittest
import unittest.mock
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
# The line of the public key of the private key `bytes(32)`.
ZERO = (
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDtqJ7zOtqQtYqOo0CpvDXNlMhV3HeJDpjrASKGLWdop"
)
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


def home(cleanup) -> str:
    """Makes a new directory the `HOME` of the process, with the login name `alice`
    and no `FOUNDATION_PROFILE`, until `cleanup` runs the cleanups that it gets. Gives
    the path of the directory."""
    dir = tempfile.TemporaryDirectory(prefix="foundation-python-")
    cleanup(dir.cleanup)
    patch = unittest.mock.patch.dict(os.environ, {"HOME": dir.name, "LOGNAME": "alice"})
    patch.start()
    cleanup(patch.stop)
    os.environ.pop("FOUNDATION_PROFILE", None)
    return dir.name


def setUpModule() -> None:
    home(unittest.addModuleCleanup)


def keygen(*arguments: str) -> str:
    """What `ssh-keygen` with `arguments` prints."""
    return subprocess.run(
        ["ssh-keygen", *arguments], check=True, capture_output=True, text=True
    ).stdout


def line(path: str) -> str:
    """The line `ssh-ed25519 <base64>` of the public key of the key file `path`."""
    return " ".join(keygen("-y", "-f", path).split()[:2])


def fingerprint(line: str) -> str:
    """The `SHA256:` fingerprint of the public key of `line`, as `ssh-keygen -l`
    prints it."""
    with tempfile.NamedTemporaryFile("w", suffix=".pub") as file:
        file.write(line + "\n")
        file.flush()
        return keygen("-l", "-f", file.name).split()[1]


# An OpenSSH key file with the public key of `ZERO` and the private key
# `bytes([1] * 32)`, whose public key is another.
MISMATCHED = """-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACA7aie8zrakLWKjqNAqbw1zZTIVdx3iQ6Y6wEihi1naKQAAAIgAAAAAAAAA
AAAAAAtzc2gtZWQyNTUxOQAAACA7aie8zrakLWKjqNAqbw1zZTIVdx3iQ6Y6wEihi1naKQ
AAAEABAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBATtqJ7zOtqQtYqOo0CpvDXNl
MhV3HeJDpjrASKGLWdopAAAAAAECAwQF
-----END OPENSSH PRIVATE KEY-----
"""


def blocks(subject: str, key: str) -> str:
    """The blocks that admit `subject` with the line `key` to read each channel."""
    return (
        f'subject "{subject}" {{\n  keys = "{key}"\n}}\n\n'
        f'access "{subject}" {{\n  allow = ["read"]\n  select = "**"\n'
        f'  subjects = "{subject}"\n}}\n'
    )


def refusal(subject: str, key: str) -> str:
    """The text of the refusal of `subject` with the line `key`."""
    return (
        "the spec has no such subject, does not list the key for it, or the "
        f"signature is not valid. If the spec of the node does not list subject "
        f"{subject} with this key, add to its config files on the node's host:\n\n"
        f"{blocks(subject, key)}\nthen run `foundation plan <files> --out <plan>` "
        "and `foundation apply <plan>` there"
    )


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
    pool and 8 MiB of disk for each core. It goes with #3064."""

    def __init__(self) -> None:
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
        line = self.process.stdout.readline()
        if not line:
            self.process.wait(PATIENCE)
            raise AssertionError(
                f"the node exited with {self.process.returncode}:\n"
                + self.process.stderr.read().decode()
            )
        self.address = self.first().address

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

    def public_key(self) -> bytes:
        """The public key of the node, as `foundation profile --json` prints it."""
        printed = subprocess.run(
            [os.environ["FOUNDATION_NODE"], "profile", "--json", self.address],
            cwd=self.dir.name,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        # The key is the last field of the SSH wire form of an Ed25519 key.
        return base64.b64decode(json.loads(printed)["public_key"].split()[1])[-32:]

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

    def apply(self, text: str) -> None:
        """Plans and applies the config file `text` on the node, as its operator
        does."""
        with open(os.path.join(self.dir.name, "admit.hcl"), "w") as file:
            file.write(text)
        plan = ["plan", "admit.hcl", "--out", "admit.plan"]
        for command in [plan, ["apply", "admit.plan"]]:
            subprocess.run(
                [os.environ["FOUNDATION_NODE"], *command],
                cwd=self.dir.name,
                check=True,
                capture_output=True,
            )

    def stop(self) -> None:
        """Stops the node with SIGTERM, and waits until it exits."""
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
        self.assertEqual(str(raised.exception), refusal(ADMIN, ZERO))

    def test_the_address_keyword_wins_over_the_data_directory(self) -> None:
        at = f"[::1]:{self.node.address.rsplit(':', 1)[1]}"
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(at, data=self.node.data, strict=True)
        profile = os.path.expanduser("~/.foundation/profile.hcl")
        self.assertEqual(
            str(raised.exception),
            f"no source gives the public key of the node at {at}, and `strict` refuses "
            f"a node that no source pins: pass public_key=, or add the block that "
            f"`foundation profile {at}` prints on the node's host to {profile}; "
            f"{self.node.data} holds the node at {self.node.address}",
        )
        dir = os.path.join(home(self.addCleanup), ".foundation")
        with self.assertRaises(ConnectionError) as raised:
            called(self, foundation.connect, at, public_key=WRONG, data=self.node.data)
        self.assertEqual(
            str(raised.exception),
            f"the session failed: no address reached peer {WRONG.hex()}; "
            f"Udp({at}): the peer stopped answering",
        )
        self.assertEqual(os.listdir(dir), ["alice.key"])

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
        self.assertEqual(str(raised.exception), refusal("plant.operator", ZERO))

    def test_a_subject_that_the_spec_does_not_list_is_refused(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            self.node.connect(subject="plant.operator", private_key=bytes(32))
        self.assertEqual(str(raised.exception), refusal("plant.operator", ZERO))

    def test_a_wrong_public_key_is_not_authenticated(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            self.node.connect(public_key=WRONG)
        self.assertEqual(
            str(raised.exception),
            f"the node proves public key {self.node.public_key().hex()}, not the "
            f"pinned {WRONG.hex()}",
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

    def test_a_private_key_with_no_subject_is_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            foundation.connect(
                "127.0.0.1:1", data=self.directory(), private_key=bytes(32)
            )
        self.assertEqual(
            str(raised.exception),
            "private_key is the key of a subject: pass subject= too",
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

    def test_no_source_of_the_node_keys_is_refused_when_strict(self) -> None:
        with self.assertRaises(ConnectionError) as raised:
            called(
                self,
                foundation.connect,
                "127.0.0.1:1",
                data=self.directory(),
                strict=True,
            )
        profile = os.path.expanduser("~/.foundation/profile.hcl")
        self.assertEqual(
            str(raised.exception),
            "no source gives the public key of the node at 127.0.0.1:1, and `strict` "
            "refuses a node that no source pins: pass public_key=, or add the block "
            "that `foundation profile 127.0.0.1:1` prints on the node's host to "
            f"{profile}",
        )
        self.assertFalse(os.path.exists(os.path.expanduser("~/.foundation")))

    def test_a_login_name_that_is_not_a_name_is_refused(self) -> None:
        with unittest.mock.patch.dict(os.environ, {"LOGNAME": "Alice Smith"}):
            with self.assertRaises(ValueError) as raised:
                foundation.connect(
                    "127.0.0.1:1", public_key=BASE, data=self.directory()
                )
        self.assertEqual(
            str(raised.exception),
            'the login name "Alice Smith" is not a subject name: a segment is not '
            'valid: "Alice Smith" in "Alice Smith"; add `subject = "<name>"` to '
            f"{os.path.expanduser('~/.foundation/profile.hcl')}, or pass subject=",
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


class ProfileTest(unittest.TestCase):
    """A program on a host with no file from the node, as its user runs it."""

    def setUp(self) -> None:
        self.dir = os.path.join(home(self.addCleanup), ".foundation")
        self.profile = os.path.join(self.dir, "profile.hcl")
        self.node = Node()
        self.addCleanup(self.node.stop)

    def pinned(self) -> str:
        """The line of the public key that the profile pins for the node."""
        with open(self.profile) as file:
            return file.read().split('public_key = "')[1].split('"')[0]

    def trusted(self, name: str, key: str, **arguments) -> None:
        """Connects to the node with no argument but its address and `arguments`, and
        asserts that the program trusts the node, pins its key, and that the node
        refuses the subject `name` with the key of the key file `key`."""
        with self.assertLogs("foundation", "WARNING") as logs:
            with self.assertRaises(foundation.Refused) as raised:
                called(self, foundation.connect, self.node.address, **arguments)
        self.assertEqual(raised.exception.code, "refused")
        self.assertEqual(str(raised.exception), refusal(name, line(key)))
        self.assertEqual(
            logs.output,
            [
                f"WARNING:foundation:trusted the node at {self.node.address} on first "
                f"use: public key {fingerprint(self.pinned())}; it is pinned in "
                f"{self.profile}"
            ],
        )

    def test_a_first_connect_trusts_the_node_and_gives_the_blocks_that_admit_it(
        self,
    ) -> None:
        key = os.path.join(self.dir, "alice.key")
        self.trusted("alice", key)
        self.assertEqual(stat.S_IMODE(os.stat(self.dir).st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(os.stat(key).st_mode), 0o600)
        with open(self.profile) as file:
            self.assertEqual(
                file.read(),
                f'node "{self.node.address}" {{\n'
                f'  public_key = "{self.pinned()}"\n}}\n',
            )
        self.node.apply(blocks("alice", line(key)))
        deadline = time.monotonic() + PATIENCE
        with self.assertNoLogs("foundation"):
            while True:
                try:
                    client = called(self, foundation.connect, self.node.address)
                    break
                except foundation.Refused:
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(0.01)
        with client:
            self.assertEqual(client.address, self.node.address)

    def test_strict_refuses_a_node_that_no_source_pins(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write("# mine\nstrict = true # no first use\n")
        refused = (
            f"no source gives the public key of the node at {self.node.address}, and "
            "`strict` refuses a node that no source pins: pass public_key=, or add the "
            f"block that `foundation profile {self.node.address}` prints on the node's "
            f"host to {self.profile}"
        )
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(str(raised.exception), refused)
        with self.assertRaises(foundation.Refused):
            with self.assertLogs("foundation", "WARNING"):
                foundation.connect(self.node.address, strict=False)
        with open(self.profile) as file:
            self.assertEqual(
                file.read(),
                f"# mine\nstrict = true # no first use\n\nnode \"{self.node.address}\" "
                f'{{\n  public_key = "{self.pinned()}"\n}}\n',
            )
        os.remove(self.profile)
        with self.assertRaises(ConnectionError) as raised:
            foundation.connect(self.node.address, strict=True)
        self.assertEqual(str(raised.exception), refused)

    def test_a_pin_that_the_node_does_not_prove_names_both_keys(self) -> None:
        with self.assertRaises(foundation.Refused):
            with self.assertLogs("foundation", "WARNING"):
                foundation.connect(self.node.address)
        proved = self.pinned()
        other = os.path.join(self.dir, "other.key")
        keygen("-q", "-t", "ed25519", "-N", "", "-f", other)
        with open(self.profile) as file:
            text = file.read()
        with open(self.profile, "w") as file:
            file.write(text.replace(proved, line(other)))
        expected = (
            f"the node at {self.node.address} proves public key "
            f"{fingerprint(proved)}, but {self.profile} pins "
            f"{fingerprint(line(other))}: when the node was made again, remove its "
            f'`node "{self.node.address}"` block from {self.profile}'
        )
        for arguments in [{"address": self.node.address}, {"data": self.node.data}]:
            with self.assertRaises(ConnectionError) as raised:
                foundation.connect(**arguments)
            self.assertEqual(str(raised.exception), expected)

    def test_the_profile_names_the_subject_and_its_key_file(self) -> None:
        dir = os.path.join(os.path.dirname(self.dir), "conf")
        os.makedirs(dir)
        profile = os.path.join(dir, "profile.hcl")
        with open(profile, "w") as file:
            file.write('subject = "plant.bob"\nprivate_key = "bob.key"\n')
        bob = os.path.join(dir, "bob.key")
        keygen("-q", "-t", "ed25519", "-N", "", "-C", "bob@laptop", "-f", bob)
        with unittest.mock.patch.dict(os.environ, {"FOUNDATION_PROFILE": profile}):
            self.profile = profile
            self.trusted("plant.bob", bob)
        self.assertFalse(os.path.exists(self.dir))

    def test_a_subject_argument_alone_takes_its_key_file(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write('subject = "alice"\nprivate_key = "mine.key"\n')
        key = os.path.join(self.dir, "plant.carol.key")
        self.trusted("plant.carol", key, subject="plant.carol")
        self.assertEqual(stat.S_IMODE(os.stat(key).st_mode), 0o600)
        self.assertFalse(os.path.exists(os.path.join(self.dir, "mine.key")))

    def test_a_key_file_with_a_passphrase_is_refused_with_its_fix(self) -> None:
        os.makedirs(self.dir)
        key = os.path.join(self.dir, "alice.key")
        keygen("-q", "-t", "ed25519", "-N", "pass", "-f", key)
        with self.assertRaises(ValueError) as raised:
            foundation.connect(self.node.address)
        passphrase = (
            "config.key-passphrase: the private key has a passphrase. Use a key file "
            'with no passphrase, as `ssh-keygen -t ed25519 -N ""` writes it. Or '
        )
        self.assertEqual(
            str(raised.exception),
            f"{key}: {passphrase}move the file away, and connect makes a new key "
            "there",
        )
        mine = os.path.join(self.dir, "mine.key")
        os.rename(key, mine)
        with open(self.profile, "w") as file:
            file.write('subject = "alice"\nprivate_key = "mine.key"\n')
        with self.assertRaises(ValueError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(
            str(raised.exception),
            f"{mine}: {passphrase}remove `private_key` from {self.profile}, and "
            f"connect makes {key}",
        )

    def test_a_key_file_of_another_problem_is_refused_with_its_fix(self) -> None:
        os.makedirs(self.dir)
        key = os.path.join(self.dir, "alice.key")
        new_key = (
            'Make a new key with `ssh-keygen -t ed25519 -N ""`, and add its `.pub` '
            "line to `keys` of the `subject` block"
        )

        def refused(expected: str) -> None:
            with self.assertRaises(ValueError) as raised:
                foundation.connect(self.node.address)
            self.assertEqual(str(raised.exception), f"{key}: {expected}")

        keygen("-q", "-t", "ed25519", "-N", "", "-f", key)
        os.replace(f"{key}.pub", key)
        refused(
            "config.key-format: the file is not an OpenSSH private key as "
            "`ssh-keygen` writes it. Use the private key file that "
            '`ssh-keygen -t ed25519 -N ""` writes, not its `.pub` file'
        )
        os.remove(key)
        keygen("-q", "-t", "ecdsa", "-N", "", "-f", key)
        refused(
            'config.key-algorithm: the private key is "ECDSA", and a subject takes '
            f"only `ssh-ed25519`. {new_key}"
        )
        with open(key, "w") as file:
            file.write(MISMATCHED)
        refused(
            "config.key-mismatch: the public key of the file is not the key of its "
            f"private key. {new_key}"
        )
        os.remove(key)
        comment = b"a\xffb"
        arguments = ["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f", key]
        subprocess.run(["ssh-keygen", *arguments], check=True)
        refused(
            "config.key-comment: the key file holds a text that is not UTF-8, such as "
            "its comment. Give the key a comment in UTF-8 with `ssh-keygen -c -C "
            "<comment> -f <file>`"
        )

    def test_a_profile_with_a_problem_is_refused_by_line(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write('subject = ""\nstrict = "no"\n')
        with self.assertRaises(ValueError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(
            str(raised.exception),
            f"{self.profile}:1:11: a name or pattern is empty. Write at least one "
            f"segment\n{self.profile}:2:10: `strict` is a bool, not a string. Write "
            "true or false",
        )

    def test_a_key_file_link_to_no_file_is_refused(self) -> None:
        os.makedirs(self.dir)
        key = os.path.join(self.dir, "alice.key")
        os.symlink(os.path.join(self.dir, "gone.key"), key)
        with self.assertRaises(OSError) as raised:
            foundation.connect(self.node.address, public_key=self.node.public_key())
        self.assertEqual(
            str(raised.exception),
            f"{key}: {os.strerror(errno.EEXIST)} (os error {errno.EEXIST}), and it "
            "names no file",
        )
        self.assertEqual(os.listdir(self.dir), ["alice.key"])

    def test_a_key_file_is_made_after_a_crash_left_its_temporary_file(self) -> None:
        os.makedirs(self.dir)
        key = os.path.join(self.dir, "alice.key")
        temporary = f"{key}.{os.getpid()}.new"
        with open(temporary, "w") as file:
            file.write("left\n")
        self.trusted("alice", key)
        self.assertFalse(os.path.exists(temporary))
        os.remove(key)
        os.mkdir(temporary)
        try:
            os.remove(temporary)
        except OSError as error:
            removed = f"{temporary}: {error.strerror} (os error {error.errno})"
        with self.assertRaises(OSError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(str(raised.exception), removed)

    def test_a_profile_private_key_in_the_home_directory(self) -> None:
        ssh = os.path.join(os.path.dirname(self.dir), ".ssh")
        os.makedirs(ssh)
        key = os.path.join(ssh, "foundation")
        keygen("-q", "-t", "ed25519", "-N", "", "-f", key)
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write('subject = "alice"\nprivate_key = "~/.ssh/foundation"\n')
        self.trusted("alice", key)
        self.assertFalse(os.path.exists(os.path.join(self.dir, "alice.key")))

    def test_a_profile_private_key_that_is_not_there_is_not_made(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write('subject = "alice"\nprivate_key = "mine.key"\n')
        with self.assertRaises(OSError) as raised:
            foundation.connect(self.node.address, public_key=self.node.public_key())
        self.assertEqual(
            str(raised.exception),
            f"{self.dir}/mine.key: no file is there: remove `private_key` from "
            f"{self.profile}, and connect makes {self.dir}/alice.key",
        )
        self.assertEqual(os.listdir(self.dir), ["profile.hcl"])

    def test_a_uid_with_no_login_name_gives_the_fix(self) -> None:
        names = ["LOGNAME", "USER", "LNAME", "USERNAME"]
        environ = dict(os.environ)
        for name in names:
            environ.pop(name, None)
        unnamed = KeyError("getpwuid(): uid not found")
        with (
            unittest.mock.patch.dict(os.environ, environ, clear=True),
            unittest.mock.patch("pwd.getpwuid", side_effect=unnamed),
        ):
            try:
                getpass.getuser()
            except (KeyError, OSError) as error:
                cause = str(error)
            with self.assertRaises(ValueError) as raised:
                foundation.connect(self.node.address, public_key=self.node.public_key())
        self.assertEqual(
            str(raised.exception),
            f"the user has no login name: {cause}; add `subject = \"<name>\"` to "
            f"{self.profile}, or pass subject=",
        )

    def test_another_error_of_the_login_name_is_raised_as_it_is(self) -> None:
        broken = RuntimeError("the password database is broken")
        with unittest.mock.patch("getpass.getuser", side_effect=broken):
            with self.assertRaises(RuntimeError) as raised:
                foundation.connect(self.node.address, public_key=self.node.public_key())
        self.assertIs(raised.exception, broken)

    def test_an_empty_profile_variable_counts_as_unset(self) -> None:
        with unittest.mock.patch.dict(os.environ, {"FOUNDATION_PROFILE": ""}):
            self.trusted("alice", os.path.join(self.dir, "alice.key"))

    def test_a_profile_that_is_not_utf8_is_refused(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "wb") as file:
            file.write(b"\xff\n")
        with self.assertRaises(ValueError) as raised:
            foundation.connect(self.node.address, public_key=self.node.public_key())
        self.assertEqual(
            str(raised.exception), f"{self.profile}: the file is not UTF-8"
        )

    def test_a_key_file_takes_bytes_that_are_not_utf8_only_after_its_end_mark(
        self,
    ) -> None:
        os.makedirs(self.dir)
        key = os.path.join(self.dir, "alice.key")
        keygen("-q", "-t", "ed25519", "-N", "", "-f", key)
        with open(key, "ab") as file:
            file.write(b"caf\xe9 notes\n")
        self.trusted("alice", key)
        with open(key, "rb") as file:
            text = file.read()
        with open(key, "wb") as file:
            file.write(b"\xe9" + text)
        with self.assertRaises(ValueError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(
            str(raised.exception),
            f"{key}: config.key-format: the file is not an OpenSSH private key as "
            "`ssh-keygen` writes it. Use the private key file that "
            '`ssh-keygen -t ed25519 -N ""` writes, not its `.pub` file',
        )

    def test_a_home_that_is_a_file_is_refused(self) -> None:
        with open(self.dir, "w") as file:
            file.write("not a directory\n")
        unread = f"{os.strerror(errno.ENOTDIR)} (os error {errno.ENOTDIR})"
        with self.assertRaises(OSError) as raised:
            foundation.connect(self.node.address)
        self.assertEqual(str(raised.exception), f"{self.profile}: {unread}")
        with self.assertRaises(OSError) as raised:
            foundation.connect(
                self.node.address, public_key=self.node.public_key(), subject="alice"
            )
        key = os.path.join(self.dir, "alice.key")
        self.assertEqual(str(raised.exception), f"{key}: {unread}")

    def test_the_profile_subject_wins_over_the_data_directory(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write('subject = "plant.bob"\n')
        with self.assertNoLogs("foundation"):
            with self.assertRaises(foundation.Refused) as raised:
                foundation.connect(data=self.node.data)
        key = os.path.join(self.dir, "plant.bob.key")
        self.assertEqual(str(raised.exception), refusal("plant.bob", line(key)))

    def test_the_public_key_argument_wins_over_a_pin(self) -> None:
        os.makedirs(self.dir)
        with open(self.profile, "w") as file:
            file.write(f'node "{self.node.address}" {{\n  public_key = "{ZERO}"\n}}\n')
        with called(
            self,
            foundation.connect,
            self.node.address,
            public_key=self.node.public_key(),
            data=self.node.data,
        ) as client:
            self.assertEqual(client.address, self.node.address)

    def test_a_pin_makes_the_directory_of_the_profile(self) -> None:
        with self.assertLogs("foundation", "WARNING"):
            with self.assertRaises(foundation.Refused) as raised:
                foundation.connect(
                    self.node.address, subject="plant.dave", private_key=bytes(32)
                )
        self.assertEqual(str(raised.exception), refusal("plant.dave", ZERO))
        self.assertEqual(stat.S_IMODE(os.stat(self.dir).st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(os.stat(self.profile).st_mode), 0o600)


if __name__ == "__main__":
    unittest.main()
