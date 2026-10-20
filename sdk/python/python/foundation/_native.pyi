from __future__ import annotations

import os
import uuid
from collections.abc import (
    ItemsView,
    Iterable,
    Iterator,
    KeysView,
    Mapping,
    ValuesView,
)
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
    session goes on. From a call, the node stopped the stream: the writer has ended,
    and each later call raises it again. Its subclass `GroupEnded` is the end of the
    write of one or more groups only. `code` is its name in lower case. At the
    open: `unknown` for a name of no channel, `not_home` when the node is not the home
    of an index, `malformed` for a reader of channels of more than one index, `failed`
    when the home cannot open the write, `refused` when the access check of the node
    refuses the write, and `busy` when the node has no room for the open. Later:
    `unknown` when a channel was removed, `failed` when the home failed, `refused` when
    the home refuses the program's proof, such as a renewal of its hello, `busy` when
    the home has no room for the outcome of a frame, and `malformed` for a frame that
    breaks a rule of the wire.
    """

    code: str

class Rejected(Exception):
    """The node refused the frame of the group of one or more indexes, and took none of
    their samples. `code` is why, for the first refused group in place order: `waiting`,
    `reserved`, `expired`, `order`, `codec`, `full`, or `large`. `written` and
    `unwritten` are the names of the channels of the frame in the groups that the node
    took and did not take, in place order. Its text names the cause of each group that
    the node did not take. The writer goes on. To write the frame again, write only the
    channels of `unwritten`: the node took those of `written`.
    """

    code: str
    written: tuple[str, ...]
    unwritten: tuple[str, ...]

class GroupEnded(Stopped):
    """The write of the group of one or more indexes ended. `code` is the end of the
    first group that ended, as `Stopped` gives it. `ended` is the names of the channels
    of the writer in each group that ended, the index included, and `written` and
    `unwritten` those of the channels of the frame in the groups that the node took and
    did not take, each in place order. Its text names the cause of each group that the
    node did not take. The writer goes on: each later frame that holds a channel of
    `ended` raises it again, so a writer of one index writes nothing more. To write the
    frame again, write only the channels of `unwritten` that are not in `ended`.
    """

    ended: tuple[str, ...]
    written: tuple[str, ...]
    unwritten: tuple[str, ...]

class DataLost(Exception):
    """Samples were lost. A writer with `on_loss="raise"` raises it when the node found
    no room for a live frame, so its samples are a gap in the log, and goes on, as a
    later frame can find room: `seqs` is a `dict` from the name of each channel of the
    lost groups of the frame, the index of each included, to the `range` of the seqs of
    its lost samples, and `frame` is `None`. A reader with `on_loss="raise"` raises it
    for a frame after a loss, and closes: `seqs` is `Frame.lost` of that frame, and
    `frame` is the frame.
    """

    seqs: dict[str, range]
    frame: Frame | None

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
        """Opens a writer of `channels` at the homes of their indexes, and blocks until
        the node opened it. `channels` is one channel name or an iterable of names, of
        any index. The channels of one index, and that index, are a group. Place `n` is
        `channels[n]`; then comes the index of each group that `channels` does not name,
        in the order of the first channel of each group. `authority` (0 to 255) is the
        writer's authority over its channels: a writer of a higher authority takes
        control from it. `lease` is the seconds that the writer may go with no write and
        keep control, or `None` for no limit. `on_loss` is what a write does when the
        node took no room for a live frame: `"warn"` logs a warning on the `foundation`
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

    def open_reader(
        self,
        channels: str | Iterable[str],
        *,
        mode: Literal["complete", "latest"] = "complete",
        on_loss: Literal["warn", "raise", "ignore"] = "warn",
    ) -> Reader:
        """Opens a reader of `channels` at the home of their index, and blocks until the
        node opened it. The reader gives only the frames written after its open, so open
        it before the writer writes. `channels` is one channel name or an iterable of
        names, all of one index. Place `n` is `channels[n]`, and the index is place
        `len(channels)` when `channels` does not name it. `mode` is `"complete"`: each
        frame from the open on, in order, past a loss of the node's buffer; or
        `"latest"`: the newest frame, which skips each frame that a newer one replaced
        before the node sent it, and loses nothing. `on_loss` is what the reader does
        with a frame after a loss: `"warn"` logs a warning on the `foundation` logger,
        `"raise"` raises `DataLost` and closes the reader, and `"ignore"` does nothing.
        `Frame.lost` holds the loss in each case. A channel of `uuid` reads as an
        `object` array, which costs one `uuid.UUID` for each sample. Other Python
        threads run while it waits.

        Raises `TypeError` for an argument of a wrong type, or for a channel of a type
        that is not a scalar; `ValueError` for no channel, a name that does not parse,
        or another `mode` or `on_loss`, with no reader opened; `Stopped` when the node
        refused the open, with a text that names each channel of the open for `unknown`;
        `ConnectionError` when the session failed; `RuntimeError` when the client closed
        or in a process that the program forked; and `PanicException` when the client's
        thread panicked.
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
    """A writer of channels of any index at their homes, which `Client.open_writer`
    opens. A `with` block closes it at its end. Its calls take turns to send: a call
    waits, with other Python threads running, while another call of the writer sends. A
    value or a logging handler that a call runs can call the writer. A process that
    forks from the program does not own the writer: in it, each call but a close raises
    `RuntimeError`, and a close gives `None` and ends nothing.
    """

    @property
    def places(self) -> tuple[tuple[str, str, int], ...]:
        """The name, the type, and the place of the index of each place, in place order,
        the type as the spec writes it (`f64`, `timestamp`). The place of an index is
        its first place, also at a repeat of it. Place `n` is channel `n` of the open;
        then comes the index of each group that the open did not name, in the order of
        the first channel of each group.
        """

    def write(self, values: dict[Any, Any] | _Value) -> None:
        """Writes one frame of one sample of each value, and blocks until the node took
        it. `values` is a `dict` from a channel name or a place to a value, or one value
        when the writer has one place that is not an index. A value at an index is the
        stamp of its group, in nanoseconds since the UNIX epoch. The values give the
        stamp of each group of the frame or of none; with none, the node stamps each
        group. A value is a `bool` or a `numpy.bool_` for `bool`, an `int` (or an object
        with `__index__`) for an integer type and for `timestamp` and `duration`
        (nanoseconds), a `float` (or an object with `__float__` or `__index__`, such as
        an `int`) for `f32` and `f64`, and a `uuid.UUID` for `uuid`. The call reads
        `values` after it waits for a call of another thread that sends. A change of
        `values` after that read is not in the frame. Other Python threads run while it
        waits.

        Raises `TypeError` for a value that is not of its place's type, a place of a
        type that is not a scalar, a key that is not a name or a place, or one value
        with no key for a writer of more places; `OverflowError` for a number outside
        its type; `KeyError` for a name of no place; `ValueError` for no value, a place
        past the places, two values of one channel, or the stamps of only some groups of
        the frame; and each error other than `TypeError` and `OverflowError` that the
        `__float__` or `__index__` of a value raised. Each `TypeError` of the conversion
        of a value is the `__cause__` of the `TypeError` that names its place. With each
        of these, nothing was sent and the writer goes on. When the node did not take
        each group of the frame, raises for the first that holds: a group whose write
        ended raises `GroupEnded`, also in a writer of one index; a refused group raises
        `Rejected`; and a lost group raises `DataLost` with `on_loss="raise"`, with the
        channels of the lost groups only. The text of each names the cause of each group
        that the node did not take. Raises `Stopped` when the node stopped the stream,
        so the writer has ended, and `ConnectionError` when the session failed, after
        each of which each call raises it again; `RuntimeError` after a close or in a
        forked process; and `PanicException` when the client's thread panicked.
        """

    def write_series(self, series: dict[Any, NDArray[Any]]) -> None:
        """Writes one frame of each series of `series`, a `dict` from a channel name or
        a place to a one-dimensional `numpy.ndarray`, and blocks until the node took it.
        The arrays of one group have one length, and the series of each index holds the
        stamps of its group. Each array has the dtype of its place in native byte order:
        `bool`, `int8` to `uint64`, `float32`, `float64`, and `int64` or
        `datetime64[ns]` for `timestamp`, `int64` or `timedelta64[ns]` for `duration`.
        The writer casts nothing, and copies each array before the send, in which other
        Python threads run. The call reads `series` after it waits for a call of another
        thread that sends. A change of `series` after that read is not in the frame.

        Raises `TypeError` for a value that is not an array of one dimension of its
        place's dtype (its text names the place, the dtypes that it takes, the dtype
        given, and the fix, such as `pass a.astype("float64")`), or a place of `uuid` or
        of a type that is not a scalar, and `ValueError` for arrays of one group with
        different lengths, an array of more than 4294967295 samples, series over the
        bytes of one frame (before their copy), no series, a group with series and no
        series of its index, a frame over the largest block, or an array that another
        call writes to. With each of these, nothing was sent and the writer goes on.
        Raises each other error as `write` raises it.
        """

    def close(self) -> None:
        """Finishes the stream, and blocks until each frame that the node took is on
        disk, or its group ended, and the node finished the stream. Control ends then. A
        close waits for a call that sends, with other Python threads running. A call
        that a close ends before its send raises `RuntimeError`. A close after a close
        gives `None`, and in a forked process it gives `None` and ends nothing.

        Raises `Stopped` when the node stopped the stream, or when the write of a group
        ended with a frame that the node took and that may not be on disk, with `code`
        the end of the first such group and a text that names the index and the end of
        each. Raises `ConnectionError` when the session failed, `RuntimeError` when the
        client closed, and `PanicException` when the client's thread panicked.
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

@final
class Reader:
    """A reader of channels of one index, which `Client.open_reader` opens. `for frame
    in reader:` gives each frame as it comes, and ends after a close. A `with` block
    closes it at its end. Read each frame as it comes: a frame that the node sent and
    the program did not read holds a part of the receive window of the client, and when
    the window is full, each writer and reader of the client waits. Read on a client of
    its own when the program cannot. A process that forks from the program does not own
    the reader: in it, each call but a close raises `RuntimeError`, and a close gives
    `None` and ends nothing.

    `next(reader)` blocks until the next frame comes, with other Python threads
    running. It runs the signal handlers (Ctrl-C) at least each 100 ms while it waits
    for a frame, and none while a `next` of another thread holds the reader, so read a
    reader on one thread. It raises `StopIteration` after a close; `DataLost` for a
    frame after a loss with `on_loss="raise"`, after which the reader is closed;
    `Stopped` with `unknown` when a channel was removed; `ConnectionError` when the
    session failed, or a frame of the node does not decode or is over the read budget
    of the client; `RuntimeError` when the client closed or in a forked process; and
    `PanicException` when the client's thread panicked. After `Stopped`,
    `ConnectionError`, or `RuntimeError` of a closed client, each later call raises it
    again.
    """

    @property
    def places(self) -> tuple[tuple[str, str, int], ...]:
        """The name, the type, and the place of the index of each place, in place order,
        the type as the spec writes it (`f64`, `timestamp`).
        """

    def __iter__(self) -> Self: ...
    def __next__(self) -> Frame: ...
    def close(self) -> None:
        """Ends the reader. A `__next__` that waits on another thread raises
        `StopIteration` within 100 ms. A close after a close gives `None`, and in a
        forked process it gives `None` and ends nothing.
        """

    def __enter__(self) -> Self: ...
    def __exit__(
        self,
        _kind: type[BaseException] | None,
        _raised: BaseException | None,
        _traceback: TracebackType | None,
    ) -> None:
        """Closes the reader."""

@final
class Frame(Mapping[str, NDArray[Any]]):
    """One frame of a reader: a read-only mapping (`collections.abc.Mapping`) from the
    name of each channel of the frame, the index included, to its series, `count`
    samples each: `frame["pt_1"]`, `"pt_1" in frame`, `for name in frame`, and
    `len(frame)`, the count of its channels. A channel that did not move in the frame is
    not a key, and a name given twice is one key. Each series is a new one-dimensional
    `numpy.ndarray` that the program owns, in native byte order: `bool`, `int8` to
    `uint64`, `float32`, `float64`, `datetime64[ns]` for `timestamp`, `timedelta64[ns]`
    for `duration`, and `object` for `uuid`, with one `uuid.UUID` for each sample. A
    later frame does not change it.
    """

    @property
    def count(self) -> int:
        """The samples of each series."""

    @property
    def lost(self) -> dict[str, range]:
        """A `dict` from each channel of the frame whose index lost seqs after the last
        series of the channel with a sample, up to this frame, to the `range` of those
        seqs: `len` of it is their count. Empty when nothing was lost, and always for a
        latest reader.
        """

    def __getitem__(self, key: str, /) -> NDArray[Any]: ...
    def __contains__(self, key: object, /) -> bool: ...
    def __iter__(self) -> Iterator[str]: ...
    def __len__(self) -> int: ...
    def keys(self) -> KeysView[str]:
        """The names of the frame, as `Mapping.keys` gives them."""

    def values(self) -> ValuesView[NDArray[Any]]:
        """The series of the frame, as `Mapping.values` gives them."""

    def items(self) -> ItemsView[str, NDArray[Any]]:
        """The pairs of each name and its series, as `Mapping.items` gives them."""

    def get(self, key: str, default: Any = None) -> NDArray[Any] | Any:
        """The series of `key`, or `default` when the frame has no series of it."""
