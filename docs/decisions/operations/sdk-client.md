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
  `ci.yaml` until #682, #2955, and #2956 give `os::clock`, `os::net`, and `os::stored`
  there (#2956: `laptop.monitor`, 2026-10-18T20:37Z,
  https://github.com/synnaxlabs/foundation/pull/3255#issuecomment-6254946471), and until
  #3294 builds its unit tests there.
  `Client::connect(&Node, Subject)` takes the node (its UDP addresses and its public
  key) apart from the subject (its name and private key): one node serves many
  subjects, and a node can listen on more than one address. `laptop.architect-2` decided
  on 2026-10-16 at 21:39 UTC
  (https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6225208118).
  Amended by #3212: `Node` holds no node key: each hello names the node by its public
  key (SUBJECT PROOF; `laptop.architect`, 2026-10-19T23:45:44Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279846281).
  `Node::local(dir)` gives the node that runs on a data directory and its first admin as
  the subject: it opens `dir/data` with `os::stored`, and runs `sector::local::read` on
  a shard as `connect` does, which logs to the caller's dispatch (`laptop.architect-2`,
  2026-10-18T16:46:56Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252485660, and
  2026-10-18T20:05:17Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6254526787).
  `Node::answering(addresses)` gives the node that answers first at `addresses` and the
  public key that it proves: one `transport::Client::dial_any`, with no hello
  (`laptop.architect-2`, 2026-10-19T23:24:32Z, item 1 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279566176), and a
  close with `Code(0)` that goes out before it returns (`laptop.architect-2`,
  2026-10-20T01:01:47Z,
  https://github.com/synnaxlabs/foundation/pull/3470#issuecomment-6280846620).
  `sdk::private_key()` gives a new key from the randomness of the OS
  (`laptop.architect-2`, 2026-10-19T23:47:10Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279865842).
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
  `Client::open_reader(read::Config)` opens a `sdk::read::Reader` of channels by name,
  all of one index. `read::Config::new` gives `Mode::Complete`. The client's thread
  opens one `program::Reader` of the names, in a task of its own. The node refuses
  channels of more than one index with `Refused(Refusal::Malformed)`, as for a writer.
  The person decided this (laptop.monitor, 2026-10-19T12:29:44Z,
  https://github.com/synnaxlabs/foundation/issues/3265#issuecomment-6267235296): the
  client-side merge by time and the probe of the index of each name leave #3265, and the
  node merges a read of more than one index in #3356, with the write half on #3283.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/3265#issuecomment-6256102155. Place
  `n` of a reader is `Config::channels[n]`, so a channel named twice is at each of its
  places. Then comes the index, when `channels` does not name it, as for a writer.
  `Frame::index`, `series`, `lost`, and `Error::Decode::place` give these places, and
  each lookup is O(1) (`laptop.architect-2`, 2026-10-19T00:18:31Z, item 1 of
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6256977605). A name of
  no channel gives `Refused(Refusal::Unknown)`: the node gives it when no name is of a
  channel, and the client when only some are, until #3356 makes the read open by name.
  The frames come in the order of their stamps (`laptop.architect-2`,
  2026-10-19T12:43:15Z,
  https://github.com/synnaxlabs/foundation/issues/3265#issuecomment-6267484776). The
  task decodes each series with `codec` into a frame that the reader keeps, with the
  lost seqs of `program::Reader::lost` (edge `sdk` -> `codec`: `laptop.architect`,
  2026-10-18T22:45:56Z, item 2 of
  https://github.com/synnaxlabs/foundation/issues/3265#issuecomment-6256109116). Each
  frame that the node sent and the caller did not read holds a part of the receive
  window of the client's session. When the window is full, each stream of the client
  waits: the answers to its writers, its other readers, and the renewal of its hello,
  and the node closes the session when the hello ends. Read each reader as its frames
  come, or read on a client of its own (item 3 of the 2026-10-19T00:18:31Z comment).
  Flow control for each reader is #3309. The frames of the readers of a client take
  blocks of a pool of their own, of 8 MiB, apart from `FRAMES`, the pool of the
  drafts of its writers, so `write_series` keeps its bound. The pool reserves about
  504 MiB of address space of the OS, with no commit charge, and gives back the pages
  of a size that it no longer uses when another size needs them, so it holds at most
  8 MiB, and up to two partial pages more of each of its sizes. On macOS, the pages of
  the free blocks of a size in use that it gives to another size stay in memory until
  the OS needs them, so the bound of 8 MiB holds only on Linux (`laptop.architect-2`,
  2026-10-19T18:36:13Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6274472373, and
  2026-10-19T18:55:15Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6274841526). When
  the OS refuses the reservation or its first page, `connect` gives `Memory`. A frame
  whose memory the OS refuses later waits in `next`, which tries again until the OS
  gives the memory or the session closes (`laptop.architect-2`, 2026-10-19T14:39:03Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6269865289,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6270455282, and
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6270555497;
  `laptop.architect` for what it states of `program`,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6270603246). A frame
  of more than 7,340,032 bytes, encoded, ends the reader with
  `Stream(Pool(TooLarge { .. }))`
  (`laptop.architect-2`, 2026-10-19T13:10:47Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6268015959; a key
  choice of kinds (d) and (8), OK of `laptop.monitor`, 2026-10-19T13:41:28Z,
  https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6268640271: a frame of
  the node is at most about 1 MiB, `BODY_MAX`). The node gives its largest frame at the
  open, and the sdk sizes the pool from it, in item 1 of #3366.
  `crates/node/benches/sdk/` gives the p50 and p99 of each round of a read. On a quiet
  host at f176f91e9
  (https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6272905066), a frame
  that comes before the call of `next` takes 14.9-16.0 µs p50 and 17.9-21.4 µs p99
  (`sdk next held`), and a call that waits before its frame comes takes the decode and
  the leg to the caller, 2.9-8.1 µs p50 and 3.9-9.9 µs p99. Both are over the P1 limit
  of 2.5 µs. Accepted as the price of SDK CLIENT, with no spin before the park. The
  purge of an idle size, whose next frame faults its pages again, is accepted too: one
  reader keeps one block of each size, which fits the budget up to the largest frame of
  the node, and several readers of one client can purge. Its bench and judgment are item
  1 of #3366. Decided by `laptop.architect-2` on 2026-10-19T18:05:17Z
  (https://github.com/synnaxlabs/foundation/pull/3306#issuecomment-6273874154).
