- **SDK PYTHON** The Python SDK is the crate `sdk-python`, a PyO3 module
  `foundation._native` of the wheel `synnax-foundation` that wraps `sdk::Client`
  (`laptop.architect`, 2026-10-17T18:41:45Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238783320;
  `laptop.architect-2`, 2026-10-17T19:07:21Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6239220977). The
  crate and the two names are key choices of kinds 1 and 2 that `laptop.monitor`
  accepted at 2026-10-17T21:32:06Z
  (https://github.com/synnaxlabs/foundation/pull/3061#issuecomment-6241427370).
  `connect(address=None, *, public_key=None, subject=None, private_key=None,
  data=None, strict=None)` dials a node (#3212 PR 1b and PR 2b). The address comes
  from the arguments, else from the data directory. Each of `public_key` and `subject`
  comes from the arguments, else from the profile, else from the data directory: on
  the data directory `data` (a `str` or an `os.PathLike`, by default
  `foundation-data`), `sdk::Node::local` gives the address and public key of the node,
  and its first admin gives the subject. The profile goes between the arguments and
  the data directory (default 1 of `laptop.architect-2`, 2026-10-15T17:01:25Z,
  https://github.com/synnaxlabs/foundation/issues/2177#issuecomment-6197670062), and
  gives no address (item 6 of
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553). With an
  address, the data directory counts only when its node is at that address. Each
  given argument parses before any read. With the address, `public_key`, and
  `subject` given, `connect` reads neither the profile nor the data directory.
  `NotRunning` is no source; `Version` and `Files` raise. `Client.address` is the
  address that `connect` dialed. The rules are in Rust, not Python.

  The profile is the HCL file that `FOUNDATION_PROFILE` names, else
  `~/.foundation/profile.hcl`, which `config::profile` decodes (PROFILE in
  `docs/decisions/config/profile.md`). An empty `FOUNDATION_PROFILE` counts as unset.
  Its `subject` gives the subject, with the key of its `private_key` file, else of
  `~/.foundation/<subject>.key`. A `private_key` path that starts with `~` expands as
  `os.path.expanduser` does, and a relative one is in the profile's directory. A file
  that the profile names and that is not there raises, and is not made (item 4 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6251937504; item 1
  of https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553). A
  `subject` argument with no `private_key` takes the key of
  `~/.foundation/<subject>.key`. With no other source, the subject is the login name
  of the user (`getpass.getuser`), with the key of `~/.foundation/<login>.key`. A key
  file in `~/.foundation` that is not there is made with mode 0600, as `ssh-keygen -t
  ed25519 -N ""` writes it, in directories made with mode 0700. Its key comes from
  `sdk::private_key` (`laptop.architect-2`, 2026-10-19T23:47:10Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279865842). The
  make removes a `<file>.<pid>.new` that a crash left, writes and syncs that file,
  links it to the name of the key file, and syncs the directory (items 2 and 3 of
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553). With
  no source of the public key, the node that answers at the address is trusted on
  first use: `sdk::Node::answering` gives its key, the profile pins it in a new
  `node` block, and the logger `foundation` gets one WARNING (`laptop.architect-2`,
  2026-10-18T15:53:21Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6251937504). The
  pin takes a lock of `<profile>.lock`, writes `<profile>.new` with mode 0600, syncs
  it, renames it, and syncs the directory, and each other byte of the file stays.
  When the profile is a link, the two siblings are of the file it links to, so the
  profile stays a link (item 2 of
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553). Under
  the lock, a `node` block of the address that is already in the file wins, and
  `connect` dials with its key. `strict`, else the profile's `strict`, refuses a node
  with no source of its key, and then `connect` writes no file. When the node refuses
  a subject with a key, other than the first admin of the data directory, the text
  gives the `subject` and `access` blocks of `config::access::admit` that admit it
  (C3 and item 4 of
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553;
  `laptop.architect-2`, 2026-10-20T01:20:04Z,
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281074157). When
  an address refuses the key of the node, `sdk::Client::connect` dials those
  addresses with no key, and gives `sdk::Error::OtherKey` when the node proves
  another key. A pinned key then gives the text of a changed key (C2 of
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553). Until
  #3476, `sdk-python` reads and writes the profile and the key file with `std::fs`
  (C1), and until #3477, it renders the problems of a profile itself (C6): each is
  deferred by `laptop.architect-2`, 2026-10-20T01:19:08Z,
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6281062553
  (`laptop.architect-2`, plan review, 2026-10-19T23:24:32Z, item 5 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279566176, and
  2026-10-19T23:49:24Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279895005). This
  supersedes the rule that `subject` and `private_key` go together, and the two texts
  of PR 1b that no source gives the public key or the subject. The texts:
  - `ValueError`: `private_key is the key of a subject: pass subject= too`. A `data`
    with a NUL byte: `data: the path holds a NUL byte`, as each other value parses
    (`laptop.architect-2`, 2026-10-18T21:14:26Z, item 5 of
    https://github.com/synnaxlabs/foundation/pull/3255#issuecomment-6255225319).
  - `ValueError`, for a profile with a problem: one line for each problem,
    `<profile>:<line>:<column>: <message>. <fix>`.
  - `ValueError`, for a login name that is not a name: ``the login name "<login>" is
    not a subject name: <cause>; add `subject = "<name>"` to <profile>, or pass
    subject=``. For a user with no login name: ``the user has no login name: <cause>;
    add `subject = "<name>"` to <profile>, or pass subject=``.
  - `ValueError`, for a profile that is not UTF-8: `<file>: the file is not UTF-8`.
  - `ValueError`, for a key file that is not the OpenSSH private key file of an
    Ed25519 key with no passphrase: `<file>: <code>: <message>. <fix>`, with the parts
    of `config::openssh::Unreadable` (PROFILE in `docs/decisions/config/profile.md`).
    For a key file with a passphrase that the profile's `private_key` names, a second
    fix follows: ``... Or remove `private_key` from <profile>, and connect makes
    ~/.foundation/<subject>.key``; for `~/.foundation/<subject>.key`: `... Or move the
    file away, and connect makes a new key there` (`laptop.architect-2`,
    2026-10-20T02:11:21Z,
    https://github.com/synnaxlabs/foundation/issues/3490#issuecomment-6281710183). The
    fix never removes the passphrase of the user's own SSH key.
  - `OSError`, for a key file that the profile names and that is not there:
    ``<file>: no file is there: remove `private_key` from <profile>, and connect
    makes ~/.foundation/<subject>.key``; for a key file that is a link to no file:
    `<file>: <cause>, and it names no file`.
  - The WARNING of trust on first use: `trusted the node at <address> on first use:
    public key <fingerprint>; it is pinned in <profile>`.
  - `Refused`, for a subject with a key: `<refusal>. If the spec of the node does not
    list subject <subject> with this key, add to its config files on the node's
    host:`, an empty line, the blocks, an empty line, and ``then run `foundation
    plan <files> --out <plan>` and `foundation apply <plan>` there``.
  - `ConnectionError`, with no address and no node on `data`: `no node runs in
    <data>: pass the address of the node, or data= with the --data of foundation
    start`.
  - `ConnectionError`, with `strict` and no source of the public key: ``no source gives
    the public key of the node at <address>, and `strict` refuses a node that no
    source pins: pass public_key=, or add the block that `foundation profile
    <address>` prints on the node's host to <profile>``, with `; <data> holds the node
    at <other>` at the end when `data` holds a node at another address.
  - `ConnectionError`, when the pinned key is not the one that the node proves: ``the
    node at <address> proves public key <fingerprint>, but <profile> pins
    <fingerprint>: when the node was made again, remove its `node "<address>"`
    block from <profile>``. With no pin, `OtherKey` gives `the node proves public key
    <hex>, not the pinned <hex>`.
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
  `connect` takes no node key: each hello names the node by its public key (SUBJECT
  PROOF; `laptop.architect`, 2026-10-19T23:45:44Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279846281).
  `sdk::Error::Thread` raises `RuntimeError`, `Bind` raises `OSError`, `Session` and
  `Local` raise `ConnectionError`, and `Panicked` raises `PanicException`
  (`laptop.architect-2`, 2026-10-17T18:44:19Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238826510). A
  `Session` that the node refused with a code raises `Refused`, a subclass of
  `ConnectionError` (SDK PYTHON WRITER in
  `docs/decisions/operations/sdk-python-writer.md`).
  `Memory`, the refusal of the memory of the pool of the frames of the readers, raises
  `MemoryError` (`laptop.architect-2`, 2026-10-19T14:39:03Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6269865289).
  `Decode`, a series of a read that does not decode, raises `ConnectionError`: it
  breaks the protocol of the node, as the `Stream` errors that map there
  (`laptop.architect-2`, 2026-10-19T12:21:56Z, item 2 of
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6267089707).
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
  wheel gives the stub `foundation/_native.pyi`, with the doc of each item, and
  `py.typed` (`laptop.architect-2`, 2026-10-19T18:31:12Z,
  https://github.com/synnaxlabs/foundation/pull/3394#issuecomment-6274374226).
  `tests/test_stubs.py` asserts that the stub defines each public item of the module,
  with its parameters, defaults, bases, `final`, and doc, that each annotation names a
  type, that each value that the module gives (a return, also `None`, a property, and
  each attribute of an exception) is of the type that the stub states, and that each
  argument of the calls that check the stub is of the type of its parameter (#3212 PR
  1c; `box2.builder-5`, plan, 2026-10-19T16:53:36Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6272502641).
  The types of the stub are a key choice of kind 2 that `laptop.monitor` accepted at
  2026-10-19T17:24Z
  (https://github.com/synnaxlabs/foundation/pull/3394#issuecomment-6274540156), with
  the defaults that let `connect()` and `open_writer("pt_1")` give no other argument.
  The stub is written by hand. A stub that PyO3 `experimental-inspect` generates lost:
  in PyO3 0.30.1 its guide calls it still in development, so its text can move with a
  PyO3 bump; it works only on an inline `#[pymodule] mod`, and a `&Bound<PyAny>`
  argument needs its type as a string in `#[pyo3(signature)]`, so the second copy of
  each type moves into Rust; and no introspection states `code` and `seqs`, which the
  module sets when it raises. The generated stub may come back when the guide of PyO3
  no longer calls the feature experimental; then `test_stubs.py` keeps only what the
  generator cannot state. The keys of the `dict` of `write` and `write_series`, and the
  values of the `dict` of `write`, are `Any` until #3408. The feature `sim` adds
  test-only functions, so a test asserts that a panic on the caller's thread gives
  `PanicException`, not an abort
  (`laptop.architect-2`, 2026-10-17T18:39:00Z,
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
