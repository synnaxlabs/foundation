- **NODE PORT (2026-10-07)** `Node::start` binds the node's one port at `Config::listen`
  on `Config::net` before any shard starts; a failed bind starts no shard, and
  `Node::join` gives `Error::Port`. The port's one part (#77) moves to shard 0, which
  builds the transport with `Config::private_key` once the last shard has opened its
  buffer (X42), so the node takes no session before that. Its limits are patches until
  #1662 makes them settings, as LIMITS of SHARD HOMES is: window 1 MiB, 64 streams of
  each kind, idle 30 s, and messages of the smaller of 64 KiB and the pool's largest
  block. Each session runs in its own future, and each stream of it reads its header in
  its own future, so a late header delays no other stream. One exhaustive `match` on
  `wire::Protocol` in `node` routes each stream; until a protocol has a server, its arm
  stops the stream with `Code(wire::header::REJECTED)` and resets the reply half with
  the same code, as for a header that does not decode, or for a first message with bytes
  after the header. The node reads no datagram until the first protocol that takes
  datagrams has a server (#1661). `Config::private_key` is a patch until `Node::start`
  reads the key from its data directory (#1660). The node admits every peer that
  completes the handshake; the mesh checks each message of a mesh stream (NODE MESH).
  With no mesh, at the stop, each session and stream future drops, then the transport.
  The bound on the wait for a header is #1628.
  Decided by `laptop.architect-2` (2026-10-07 21:09 UTC):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669, on the
  plan https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046861267;
  datagrams and the key, by `laptop.architect-2` (2026-10-07 23:26 UTC):
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048898047.
  Amended (2026-10-07, #1649 round 1, a finding of `performance` that
  `laptop.integrator-1` deferred, 23:41 UTC): the window caps a session at the window
  over the round trip, about 21 MB/s at 50 ms, until #1662 sizes it from the
  bandwidth-delay product. The number of sessions has no bound until #1628.
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6049077609.
  Amended (2026-10-08, #1647, by `laptop.architect-2`, 00:05 UTC): a transport that
  stops with an error stops the node, and `Node::join` gives `Error::Transport`. The
  node does not rebind the port:
  https://github.com/synnaxlabs/foundation/issues/1647#issuecomment-6049354544.
  Supersedes the deferral of
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048464411
  (2026-10-07 22:50 UTC), under which the node ran on with no port until #1647.
  Amended (2026-10-08, PR 3b of #585, by `laptop.architect`, 03:37 UTC): shard 0 opens
  the mesh before the first session. `route` gives each `Mesh` stream of a
  `Peer::Node` session to `Mesh::serve` with that key, never a key from a header or a
  message. A `Mesh` stream of a `Peer::Client` session stops with
  `Code(wire::header::REJECTED)`, and its reply half resets with the same code. For
  `Mesh` streams, the admission rule is the mesh's check of each message: a peer that
  is not the member it names gets `Spoofed`, and the stream stops at that message. The
  `Hub` rule comes with PR 4. At the stop, each session and stream future drops, then
  the mesh, and shard 0 waits for the mesh's task to end before it drops `lock` (DATA
  DIRECTORY LOCK) and before `Node::join` returns, so a restart at once opens the log:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475. For a
  node with a mesh, this supersedes the stop order of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669.
  Amended (2026-10-08, #1830, by `laptop.architect-2`, 07:26 UTC): with a mesh, each
  session and stream future drops, then the mesh, and the transport drops when the
  last task of the mesh ends, before `lock` drops:
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235. Amended
  again (2026-10-08, #1962, by `laptop.architect-2`, 20:07 UTC): with a mesh, shard 0
  waits for each task of the mesh to end, and the transport drops with the last of the
  port's future and the tasks of the mesh:
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6068113010. This
  holds also when the mesh's group stops before the node, which then stops the node
  (NODE MESH), by `laptop.architect-2` (21:31 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069443456.
  Supersedes the #1780 clause of
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6068113010. Under
  `sim`, the port is free once `lock` is, which a test pins. Under `os`, the carrier's
  task can hold the socket after `lock` drops, until the runtime of shard 0 drops,
  before `Node::join` returns: with a socket that works, the task ends only once each
  connection drained and the runtime polls it. #2017 makes shard 0 wait for the task
  before it drops `lock`, by `laptop.architect-2` (21:58 UTC, words of 22:23 UTC and
  22:47 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069829972,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6070247529,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6070577797.
  Supersedes the port clause of
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069568312. No test
  sees the drop of the transport itself: the end of shard 0 drops the carrier's task,
  which frees the socket also when a clone of the transport leaks, by
  `laptop.architect-2` (21:39 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069568312. Supersedes
  the drop of the transport of
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235.
