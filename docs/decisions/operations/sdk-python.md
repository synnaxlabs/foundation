- **SDK PYTHON** The Python SDK is the crate `sdk-python`, a PyO3 module
  `foundation._native` of the wheel `synnax-foundation` that wraps `sdk::Client`
  (`laptop.architect`, 2026-10-17T18:41:45Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238783320;
  `laptop.architect-2`, 2026-10-17T19:07:21Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6239220977). The
  crate and the two names are key choices of kinds 1 and 2 that `laptop.monitor`
  accepted at 2026-10-17T21:32:06Z
  (https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241427370).
  `connect(address=None, *, node=None, public_key=None, subject=None,
  private_key=None, data=None)` dials a node (#3212 PR 1b). Each of the address, the
  pair (`node`, `public_key`), and the pair (`subject`, `private_key`) comes from the
  arguments, else from `sdk::Node::local` on the data directory `data` (a `str` or an
  `os.PathLike`, by default `foundation-data`): the node gives its address and keys,
  and its first admin gives the subject. With an address, the data directory counts
  only when its node is at that address. Each given argument parses before any read,
  and with every value given, `connect` reads no file. `NotRunning` is no source;
  `Version` and `Files` raise. When no source gives the keys and the subject, the text
  of the keys wins. `Client.address` is the address that `connect` dialed. The rules
  are in Rust, not Python. A profile of PR 2 goes between the arguments and the data
  directory (default 1 of `laptop.architect-2`, 2026-10-15T17:01:25Z,
  https://github.com/synnaxlabs/foundation/issues/2177#issuecomment-6197670062). The
  texts:
  - `ValueError`: `node and public_key go together: give both or neither`, and
    `subject and private_key go together: give both or neither`. A `data` with a NUL
    byte: `data: the path holds a NUL byte`, as each other value parses
    (`laptop.architect-2`, 2026-10-18T21:14:26Z, item 5 of
    https://github.com/synnaxlabs/foundation/pull/3255#issuecomment-6255225319).
  - `ConnectionError`, with no address and no node on `data`: `no node runs in
    <data>: pass the address of the node, or data= with the --data of foundation
    start`.
  - `ConnectionError`, with no source of the keys: `no source gives the keys of the
    node at <address>: pass node= and public_key=, or data= with the --data of
    foundation start on the node's host`. When `data` holds a node at another address:
    `no source gives the keys of the node at <address>: <data> holds the node at
    <other>; pass node= and public_key=, or connect to <other>`.
  - `ConnectionError`, with no source of the subject: `no source gives the subject for
    the node at <address>: pass subject= and private_key=, or data= with the --data of
    foundation start on the node's host`, and the same form with `<data> holds the
    node at <other>; pass subject= and private_key=, or connect to <other>`.
  - `ConnectionError`, when the subject must come from a node with no `admin.key`:
    `<data> holds no admin.key that a node wrote`.
  - `ConnectionError` for `Version`: `<data>/data/<file> holds format version
    <found>, and this package reads version <expected>: install the package of the
    node's version`.
  - `ConnectionError` for `Files`: `cannot read <data>/data: <cause>`. Each path of
    the cause is relative to `<data>/data`, and `.` is that directory
    (`laptop.architect-2`, 2026-10-18T20:42:17Z, item 3 of
    https://github.com/synnaxlabs/foundation/pull/3255#issuecomment-6254906786).

  (`box1.builder-1`, plan, 2026-10-18T20:01:07Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6254483901;
  `laptop.architect-2`, 2026-10-18T16:46:56Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252485660,
  2026-10-18T17:28:39Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252920038,
  2026-10-18T17:34:06Z,
  https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6252975559, and
  2026-10-18T20:05:17Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6254526787.)
  `connect` takes the node key as a `uuid.UUID`, and a value of another type raises
  `TypeError`.
  `sdk::Error::Thread` raises `RuntimeError`, `Bind` raises `OSError`, `Session` and
  `Local` raise `ConnectionError`, and `Panicked` raises `PanicException`
  (`laptop.architect-2`, 2026-10-17T18:44:19Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238826510). A
  `Session` that the node refused with a code raises `Refused`, a subclass of
  `ConnectionError` (SDK PYTHON WRITER in
  `docs/decisions/operations/sdk-python-writer.md`).
  No Python test reaches `Bind`, so the Rust table test of the map, with a real `Bind`,
  is its only test (`laptop.architect-2`, 2026-10-18T15:00:03Z,
  https://github.com/synnaxlabs/foundation/pull/3193#issuecomment-6251395290).
  `Client.__exit__` lets the error of the block win: when the block raised, an error of
  the close is a note on it, else the error of the close raises (`laptop.architect-2`,
  2026-10-17T18:46:12Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238857409). A
  close during another close waits for it to end and gives `None`, and only the close
  that ends the session raises. The binding keeps the closed state, and `sdk` does not
  change (`laptop.architect-2`, 2026-10-17T20:59:42Z,
  https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6240961945). A
  process that forks from the program does not own the client: in it, a close gives
  `None`, and neither a close nor a drop ends the session (`laptop.architect-2`,
  2026-10-17T21:42:45Z,
  https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241627570). In a
  forked process, each later call of `Client` that sends to the client's thread raises
  `RuntimeError` at once, with a text that names the process that connected and says to
  connect again in this process. The first PR with such a call adds the check and a
  test that forks (`laptop.monitor`'s condition, 2026-10-17T21:43:25Z,
  https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241641584). The
  surface and the errors are key choices of kinds 2 and 9 that `laptop.monitor`
  accepted at 2026-10-17T18:44:46Z
  (https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6240907801). This
  surface is a step: the demo needs `foundation.connect("address")` alone, so the
  defaults of #2177 land before it
  (https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238857409). The
  feature `sim` adds test-only functions, so a test asserts that a panic on the caller's
  thread gives `PanicException`, not an abort (`laptop.architect-2`,
  2026-10-17T18:39:00Z,
  https://github.com/synnaxlabs/foundation/issues/3059#issuecomment-6238735903). One
  Rust test runs the suite `sdk/python/tests` in an embedded interpreter on the build
  of the crate, so `cargo test` and `cargo mutants` test the module. It starts the
  binary at `FOUNDATION_NODE`, and the crate has no edge to `node`. The crate does not
  turn on the pyo3 feature `extension-module`, since maturin sets
  `PYO3_BUILD_EXTENSION_MODULE` (`laptop.architect-2`, 2026-10-17T21:38:45Z,
  https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241548882 and
  https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241559479). In the CI
  jobs that run `cargo test`, `FOUNDATION_NODE` is a debug `foundation` that the job
  builds: the suite needs a node process, not a fast one, and a test that fails on a
  debug node has a time bound that is too tight (`laptop.architect-2`,
  2026-10-17T22:04:19Z,
  https://github.com/synnaxlabs/foundation/pull/3069#issuecomment-6241909398). One
  `abi3` wheel for each platform takes Python 3.11 and later, and does not install on
  free-threaded Python. The Linux wheels are `manylinux_2_28`, with no `musllinux`
  wheel. The gate runs against a release node built in the same run. `laptop.monitor`
  accepted both, key choices of kinds (a) and (g), at 2026-10-17T18:40:34Z
  (https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6240907801). The
  Windows wheels wait for #682 and #2955 (option (A),
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238826510), and
  for the disk of a data directory (#2956), with no `cfg` gate on `data=`
  (`laptop.monitor`, 2026-10-18T20:37Z, a key choice of kind (a),
  https://github.com/synnaxlabs/foundation/pull/3255#issuecomment-6254946471).
