from __future__ import annotations

import os
import uuid
from collections.abc import Iterable
from types import TracebackType
from typing import (
    Any,
    Literal,
    Self,
    SupportsFloat,
    SupportsIndex,
    TypeAlias,
    final,
)

import numpy
from numpy.typing import NDArray

_Float: TypeAlias = SupportsFloat | SupportsIndex
_Value: TypeAlias = numpy.bool_ | _Float | uuid.UUID

def connect(
    address: str | None = None,
    *,
    public_key: bytes | None = None,
    subject: str | None = None,
    private_key: bytes | None = None,
    data: str | os.PathLike[str] | None = None,
    strict: bool | None = None,
) -> Client:
    """Dials the node at `address` (`ip:port`, UDP), and blocks until it admits the
    first hello of `subject`. The client then renews the hello until it closes.

    `public_key` is the 32 bytes of the key that the node proves, and `private_key` the
    32 bytes of a key that the spec lists for `subject`. A `subject` with no
    `private_key` takes the key of `~/.foundation/<subject>.key`. The address comes from
    the arguments, else from the data directory. Each of `public_key` and `subject`
    comes from the arguments, else from the profile, else from the data directory. The
    data directory is `data`, a `str` or an `os.PathLike`, the `--data` of `foundation
    start`, by default `foundation-data`. The node that runs on it gives its address and
    public key, and its first admin gives the subject. With an `address`, the data
    directory counts only when its node is at that address: `127.0.0.1:p` matches a node
    bound on `0.0.0.0:p`.

    The profile is the HCL file that `FOUNDATION_PROFILE` names, else
    `~/.foundation/profile.hcl`. Its `subject` gives the subject, with the key of its
    `private_key` file, else of `~/.foundation/<subject>.key`. A `private_key` path that
    starts with `~/` is in the home directory, and a relative one is in the directory of
    the profile's path, also when the profile is a link. Each `node` block pins the
    public key of the node at its address. With no other source, the subject is the
    login name of the user, with the key of `~/.foundation/<login>.key`. A key file in
    `~/.foundation` that is not there is made, as `ssh-keygen -t ed25519 -N ""` makes
    it. With no source of the public key, the node that answers at `address` is trusted
    on first use: the profile pins its key, and the logger `foundation` gets one
    WARNING. `strict`, else the profile's `strict`, refuses a node with no source of its
    key. Other Python threads run while it waits.

    Raises `TypeError` for an argument of a wrong type, `ValueError` for an argument
    that does not parse, a `private_key` with no `subject`, a profile with a problem, no
    login name or one that is not a subject name, a profile that is not UTF-8, or a key
    file that is not the OpenSSH file of an Ed25519 key with no passphrase,
    `RuntimeError` when a thread of the client or of the read of `data` cannot start,
    `OSError` when the local UDP socket cannot bind, the profile or a key file cannot be
    read or written, the profile names a key file that is not there, or the user has
    no home directory for a `~/` path that it needs, `MemoryError` when the OS
    refuses the memory of the pool of the frames of the client's readers,
    `ConnectionError` when no source gives the address, or with `strict` the public key,
    the data directory cannot be read, the client cannot reach the node, the node proves
    another key than its source gives, or the node refuses the subject, and
    `PanicException` when a thread of the client or of the read of `data` panicked.
    """

class Refused(ConnectionError):
    """The node refused the session or closed it, with a code: connect again. `code` is
    its name in lower case: `refused` when the spec does not list the subject or its
    key, `unsynced` when the node has no mesh time yet, `stale`, `via`, `expired`, or
    `changed` for a hello that the node refused, or a code of `Stopped`.
    """

    code: str

class Stopped(Exception):
    """The node stopped a write, with a code. At the open, no writer opened, and the
    session goes on. A writer that raises it has ended, and each later call raises it
    again. `code` is its name in lower case. At the open: `unknown` for a name of no
    channel, `not_home` when the node is not the home of the index, `malformed` for
    channels of more than one index or a lease that the home refuses, `failed` when the
    home cannot open the write, `refused` when the access check of the node refuses the
    write, and `busy` when the node has no room for the open. Later: `unknown` when a
    channel was removed, `failed` when the home failed, `busy` when the home has no room
    for the outcome of a frame, and `malformed` for a frame that breaks a rule of the
    wire.
    """

    code: str

class Rejected(Exception):
    """The node refused one frame of a writer, and took none of it. `code` is why:
    `waiting`, `reserved`, `expired`, `order`, `codec`, `full`, or `large`. The writer
    goes on.
    """

    code: str

class DataLost(Exception):
    """The node found no room for a live frame, so its samples are a gap in the log.
    `seqs` is a `dict` from the name of each channel of the frame to the `range` of the
    seqs of its lost samples. The writer goes on, as a later frame can find room.
    """

    seqs: dict[str, range]

@final
class Client:
    """A program's session with one node. A `with` block closes it at its end. A client
    that the program drops without a close ends its session and does not wait. A
    process that forks from the program does not own the client: in it, a close gives
    `None`, and neither a close nor a drop ends the session.
    """

    @property
    def address(self) -> str:
        """The address that `connect` dialed, as `ip:port`."""

    def close(self) -> None:
        """Closes the session, and blocks until the close goes out, the transport drains
        each close (up to 3 s), and the client's thread ends. A close during another
        close waits for it to end, and gives `None`, as a close after it does. A close
        during an `open_writer` of another thread also waits for that open to end.

        Raises `ConnectionError` with the cause when the session ended before the close,
        and `PanicException` when the client's thread panicked. Only the close that ends
        the session raises.
        """

    def open_writer(
        self,
        channels: str | Iterable[str],
        *,
        authority: int = 0,
        lease: _Float | None = None,
        on_loss: Literal["warn", "raise", "ignore"] = "warn",
    ) -> Writer:
        """Opens a writer of `channels` at the home of their index, and blocks until the
        node opened it. `channels` is one channel name or an iterable of names, all of
        one index. Place `n` is `channels[n]`, and the index is place `len(channels)`
        when `channels` does not name it. `authority` (0 to 255) is the writer's
        authority over its channels: a writer of a higher authority takes control from
        it. `lease` is the seconds that the writer may go with no write and keep
        control, or `None` for no limit. `on_loss` is what a write does when the node
        took no room for a live frame: `"warn"` logs a warning on the `foundation`
        logger, `"raise"` raises `DataLost`, and `"ignore"` does nothing. Other Python
        threads run while it waits.

        Raises `TypeError` for an argument of a wrong type; `ValueError` for no channel,
        a name that does not parse, an `authority` outside 0 to 255, a `lease` that,
        taken as a `float`, does not round to 1 to 2^63 - 1 nanoseconds, or another
        `on_loss`, with no writer opened; `Stopped` when the node refused the open, with
        a text that names each channel of the open for `unknown`; `ConnectionError` when
        the session failed; `RuntimeError` when the client closed or in a process that
        the program forked; and `PanicException` when the client's thread panicked.
        """

    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        _kind: type[BaseException] | None,
        raised: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        """Closes the client. When the block raised, an error of the close is a note on
        the error of the block; else it raises.
        """

@final
class Writer:
    """A writer of channels of one index at its home, which `Client.open_writer` opens.
    A `with` block closes it at its end. Its calls take turns to send: a call waits,
    with other Python threads running, while another call of the writer sends. A value
    or a logging handler that a call runs can call the writer. A process that forks from
    the program does not own the writer: in it, each call but a close raises
    `RuntimeError`, and a close gives `None` and ends nothing.
    """

    @property
    def places(self) -> tuple[tuple[str, str], ...]:
        """The name and the type of each place, in place order, the type as the spec
        writes it (`f64`, `timestamp`).
        """

    @property
    def index(self) -> int:
        """The place of the index, whose samples are the stamps of each frame."""

    def write(self, values: dict[Any, Any] | _Value) -> None:
        """Writes one frame of one sample of each value, and blocks until the node took
        it. `values` is a `dict` from a channel name or a place to a value, or one value
        when the writer has one place other than the index. A value at the index is the
        stamp of the frame, in nanoseconds since the UNIX epoch; with none, the node
        stamps the frame. A value is a `bool` or a `numpy.bool_` for `bool`, an `int`
        (or an object with `__index__`) for an integer type and for `timestamp` and
        `duration` (nanoseconds), a `float` (or an object with `__float__` or
        `__index__`, such as an `int`) for `f32` and `f64`, and a `uuid.UUID` for
        `uuid`. The call reads `values` after it waits for a call of another thread that
        sends. A change of `values` after that read is not in the frame. Other Python
        threads run while it waits.

        Raises `TypeError` for a value that is not of its place's type, a place of a
        type that is not a scalar, a key that is not a name or a place, or one value
        with no key for a writer of more places; `OverflowError` for a number outside
        its type; `KeyError` for a name of no place; `ValueError` for no value, a place
        past the places, or two values of one channel; and each error other than
        `TypeError` and `OverflowError` that the `__float__` or `__index__` of a value
        raised. Each `TypeError` of the conversion of a value is the `__cause__` of the
        `TypeError` that names its place. With each of these, nothing was sent and the
        writer goes on. Raises `Rejected` when the node refused the frame, and
        `DataLost` for a lost frame with `on_loss="raise"`; the writer goes on after
        both. Raises `Stopped` when the node stopped the stream, and `ConnectionError`
        when the session failed, after which each call raises it again; `RuntimeError`
        after a close or in a forked process; and `PanicException` when the client's
        thread panicked.
        """

    def write_series(self, series: dict[Any, NDArray[Any]]) -> None:
        """Writes one frame of each series of `series`, a `dict` from a channel name or
        a place to a one-dimensional `numpy.ndarray`, and blocks until the node took it.
        Each array has the same length, and the series of the index holds the stamps.
        Each array has the dtype of its place in native byte order: `bool`, `int8` to
        `uint64`, `float32`, `float64`, and `int64` or `datetime64[ns]` for `timestamp`,
        `int64` or `timedelta64[ns]` for `duration`. The writer casts nothing, and
        copies each array before the send, in which other Python threads run. The call
        reads `series` after it waits for a call of another thread that sends. A change
        of `series` after that read is not in the frame.

        Raises `TypeError` for a value that is not an array of one dimension of its
        place's dtype (its text names the place, the dtypes that it takes, the dtype
        given, and the fix, such as `pass a.astype("float64")`), or a place of `uuid` or
        of a type that is not a scalar, and `ValueError` for arrays of different
        lengths, an array of more than 4294967295 samples, series over the bytes of one
        frame (before their copy), no series, no series of the index, a frame over the
        largest block, or an array that another call writes to.
        With each of these, nothing was sent and the writer goes on. Raises each other
        error as `write` raises it.
        """

    def close(self) -> None:
        """Finishes the stream, and blocks until each frame that the node took is on
        disk, or its group ended, and the node finished the stream. Control ends then.
        A close waits for a call that sends, with other Python threads running. A call
        that a close ends before its send raises `RuntimeError`. A close after a close
        gives `None`, and in a forked process it gives `None` and ends nothing.

        Raises `Stopped` when the node stopped the stream or the write of its group
        ended, and then a frame that the node took may not be on disk. Raises
        `ConnectionError` when the session failed, `RuntimeError` when the client
        closed, and `PanicException` when the client's thread panicked.
        """

    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        _kind: type[BaseException] | None,
        raised: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        """Closes the writer. When the block raised, an error of the close is a note on
        the error of the block; else it raises.
        """
