- **SDK CLIENT** `sdk::Client` is a blocking client that owns one thread, with a Tokio
  `LocalRuntime`. The thread holds a `program::Session` on a `transport::Client`, so the
  `!Send` session stays out of the surface, and `sdk` builds on no node core (`hub`,
  `home`, `mesh`, `raft`). The subject's key is `private_key`. `laptop.architect-2`
  decided on 2026-10-16 at 21:06 UTC
  (https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6224695365).
  `laptop.architect` approved the boundary (2026-10-16T21:33:33Z,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225122027). Amended
  (2026-10-17T00:22:08Z, `laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6227271021): `sdk`
  takes its clock, entropy, and network from `os::clock()`, `os::entropy()`, and
  `os::net()`, as the node does, so `sdk` touches the OS only through `os`.
  `laptop.architect` approved the edge `sdk` -> `os` (2026-10-17T17:03:12Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237211963). The
  client's thread is a shard of `os::shards()`, named `foundation-sdk`, with no core
  (`laptop.architect-2`, 2026-10-17T17:32:45Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237701374). The
  shard logs to the dispatch of the caller of `connect`, so the client's logs go where
  the program's logs go (`laptop.architect-2`, 2026-10-17T18:19:03Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6238455226; a key
  choice of kind 9, finding 2 of
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6238447555). This
  replaces the env of its own on `Instant`, `getrandom`, and `socket2`, so #2932 closes:
  `os::clock` counts time while the host sleeps. `sdk` is on the Windows lists of
  `ci.yaml` until #682 and #2955 give `os::clock` and `os::net` there.
  `Client::connect(&Node, Subject)` takes the node (its UDP addresses, its key, and its
  public key) apart from the subject (its name and private key): one node serves many
  subjects, and a node can listen on more than one address. `laptop.architect-2` decided
  on 2026-10-16 at 21:39 UTC
  (https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225208118).
  `Client::close` blocks until the close of the session goes out, the transport drains
  each close (up to 3 s), and the thread ends. Drop only signals the end, and a panic of
  the thread after a drop goes to the panic hook of the process (`laptop.architect-2`,
  2026-10-16T21:46:22Z,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225303374). In a
  build where panics unwind, a panic of the client's thread gives `Error::Panicked` from
  `connect` or `close`, and the panic hook of the process gets its message
  (`laptop.architect-2`, 2026-10-17T17:32:45Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237701374 and
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237729706). The wheel
  builds with the profile `wheel`, `release` with `panic = "unwind"`, so a panic in the
  client gives the Python caller an exception, not an abort (`laptop.architect`,
  2026-10-17T17:33:46Z,
  https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237722519). It lands
  with the wheel build in PR 1b of #2923. A failure of the session after `connect`
  stays silent until the next call, and that call gives the cause of the failure, never
  a generic closed error
  (`laptop.monitor`, key choice of kind 9, 2026-10-16T22:40:46Z,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6226048578). The next
  call, `close` too, gives the cause: `close` gives `Error::Session` with it when the
  session ended before the close (`laptop.architect-2`, 2026-10-16T22:20:16Z,
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225794091).
